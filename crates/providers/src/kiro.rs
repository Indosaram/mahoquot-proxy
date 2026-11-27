//! Kiro accounts.
//!
//! Contract mirrored from kiro-lb `kiro/auth.py` and `kiro/config.py`. Kiro has
//! TWO refresh paths chosen by how the account was created — desktop social
//! login versus AWS SSO OIDC — so a credential carries its auth mode and the
//! region is part of the refresh host, not a header.
//!
//! The API host is deliberately region-templated but universal: per kiro-lb
//! issue #58, `codewhisperer.{region}.amazonaws.com` does not resolve outside
//! us-east-1, so `runtime.{region}.kiro.dev` is the correct host for every
//! region.

use std::path::{Path, PathBuf};
use std::sync::{OnceLock, RwLock};

use crate::account::LoadError;

pub const KIRO_DEFAULT_REGION: &str = "us-east-1";
pub const KIRO_CLI_DEFAULT_VERSION: &str = "3.6.2";
pub const KIRO_KAS_DEFAULT_VERSION: &str = "0.66.127-hotfix.1";
pub const KIRO_KAS_VERSION: &str = KIRO_KAS_DEFAULT_VERSION;
pub const KIRO_CLI_MANIFEST_URL: &str =
    "https://prod.download.cli.kiro.dev/stable/latest/manifest.json";
pub const KIRO_CLI_ORIGIN: &str = "KIRO_CLI";
pub const KIRO_FEATURE_CONFIG_SALT: &str = "kiro-feature-config-key-salt-3e9b1d7a";

pub const KIRO_SOCIAL_REFRESH_TEMPLATE: &str =
    "https://prod.{region}.auth.desktop.kiro.dev/refreshToken";
pub const KIRO_IDC_REFRESH_TEMPLATE: &str = "https://oidc.{region}.amazonaws.com/token";
pub const KIRO_API_HOST_TEMPLATE: &str = "https://runtime.{region}.kiro.dev";
pub const KIRO_MANAGEMENT_HOST_TEMPLATE: &str = "https://management.{region}.kiro.dev";

pub const KIRO_GENERATE_PATH: &str = "/generateAssistantResponse";
pub const KIRO_RESPONSES_PATH: &str = "/v1/responses";
pub const KIRO_FEATURE_CONFIG_PATH: &str = "/featureConfiguration";
pub const KIRO_USAGE_LIMITS_PATH: &str = "/Get-Usage-Limits";

static LATEST_KIRO_CLI_VERSION: OnceLock<RwLock<String>> = OnceLock::new();
static LATEST_KIRO_KAS_VERSION: OnceLock<RwLock<String>> = OnceLock::new();

fn latest_version_store() -> &'static RwLock<String> {
    LATEST_KIRO_CLI_VERSION.get_or_init(|| RwLock::new(KIRO_CLI_DEFAULT_VERSION.to_string()))
}

fn latest_kas_store() -> &'static RwLock<String> {
    LATEST_KIRO_KAS_VERSION.get_or_init(|| RwLock::new(KIRO_KAS_DEFAULT_VERSION.to_string()))
}

pub fn current_kiro_cli_version() -> String {
    latest_version_store()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

pub fn current_kiro_kas_version() -> String {
    latest_kas_store()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

pub fn update_kiro_kas_version(candidate: &str) -> bool {
    let trimmed = candidate.trim().trim_start_matches('v');
    if trimmed.is_empty()
        || trimmed.len() > 64
        || !trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
        || !trimmed.bytes().next().is_some_and(|b| b.is_ascii_digit())
    {
        return false;
    }
    let mut guard = latest_kas_store()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if *guard == trimmed {
        return false;
    }
    *guard = trimmed.to_string();
    true
}

fn parse_semver_parts(version: &str) -> Option<Vec<u32>> {
    let parts: Vec<&str> = version.trim().split('.').collect();
    if !(2..=4).contains(&parts.len()) {
        return None;
    }
    let mut nums = Vec::with_capacity(parts.len());
    for part in parts {
        if part.is_empty() || part.len() > 5 || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let value = part.parse::<u32>().ok()?;
        if value.to_string() != part {
            return None;
        }
        nums.push(value);
    }
    Some(nums)
}

pub fn is_newer_kiro_version(candidate: &str, baseline: &str) -> bool {
    let Some(lhs) = parse_semver_parts(candidate) else {
        return false;
    };
    let Some(rhs) = parse_semver_parts(baseline) else {
        return true;
    };
    let len = lhs.len().max(rhs.len());
    for idx in 0..len {
        let a = lhs.get(idx).copied().unwrap_or(0);
        let b = rhs.get(idx).copied().unwrap_or(0);
        if a != b {
            return a > b;
        }
    }
    false
}

pub fn update_kiro_cli_version(candidate: &str) -> bool {
    let trimmed = candidate.trim().trim_start_matches('v');
    let mut guard = latest_version_store()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if !is_newer_kiro_version(trimmed, &guard) {
        return false;
    }
    *guard = trimmed.to_string();
    true
}

const ZSTD_MAGIC: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
const MAX_KAS_PACKAGE_DOWNLOAD_BYTES: usize = 96 * 1024 * 1024;
const MAX_KAS_PACKAGE_JSON_BYTES: usize = 256 * 1024;

#[derive(Default)]
struct TarPackageJsonScanner {
    header_buf: Vec<u8>,
    remaining_entry_bytes: usize,
    remaining_pad_bytes: usize,
    capturing: bool,
    captured: Vec<u8>,
    found_version: Option<String>,
}

impl TarPackageJsonScanner {
    fn feed(&mut self, mut data: &[u8]) -> Option<String> {
        if let Some(ref version) = self.found_version {
            return Some(version.clone());
        }
        while !data.is_empty() {
            if self.remaining_entry_bytes > 0 {
                let take = data.len().min(self.remaining_entry_bytes);
                if self.capturing {
                    self.captured.extend_from_slice(&data[..take]);
                }
                self.remaining_entry_bytes -= take;
                data = &data[take..];
                if self.remaining_entry_bytes == 0 && self.capturing {
                    self.capturing = false;
                    if let Ok(parsed) = serde_json::from_slice::<serde_json::Value>(&self.captured) {
                        if let Some(ver) = parsed.get("version").and_then(serde_json::Value::as_str) {
                            if !ver.trim().is_empty() {
                                self.found_version = Some(ver.trim().to_string());
                                return self.found_version.clone();
                            }
                        }
                    }
                    self.captured.clear();
                }
                continue;
            }
            if self.remaining_pad_bytes > 0 {
                let take = data.len().min(self.remaining_pad_bytes);
                self.remaining_pad_bytes -= take;
                data = &data[take..];
                continue;
            }
            let need = 512 - self.header_buf.len();
            let take = data.len().min(need);
            self.header_buf.extend_from_slice(&data[..take]);
            data = &data[take..];
            if self.header_buf.len() < 512 {
                break;
            }
            let hdr = std::mem::take(&mut self.header_buf);
            if hdr.iter().all(|&b| b == 0) {
                continue;
            }
            let name_end = hdr[..100].iter().position(|&b| b == 0).unwrap_or(100);
            let name = String::from_utf8_lossy(&hdr[..name_end]);
            let prefix_end = hdr[345..500].iter().position(|&b| b == 0).unwrap_or(155);
            let prefix = String::from_utf8_lossy(&hdr[345..345 + prefix_end]);
            let full_name = if prefix.is_empty() {
                name.into_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let size_end = hdr[124..136].iter().position(|&b| b == 0).unwrap_or(12);
            let size_str = std::str::from_utf8(&hdr[124..124 + size_end])
                .unwrap_or("")
                .trim();
            let size = usize::from_str_radix(size_str, 8).unwrap_or(0);
            let pad = (512 - (size % 512)) % 512;
            let typeflag = hdr[156];
            let is_regular = typeflag == 0 || typeflag == b'0';
            let target_file = is_regular
                && (full_name.ends_with("@kiro/agent/package.json")
                    || full_name == "package.json")
                && size > 0
                && size <= MAX_KAS_PACKAGE_JSON_BYTES;
            self.remaining_entry_bytes = size;
            self.remaining_pad_bytes = pad;
            self.capturing = target_file;
            if target_file {
                self.captured.clear();
                self.captured.reserve(size);
            }
        }
        self.found_version.clone()
    }
}

struct InnerCandidate {
    dec: zstd::stream::raw::Decoder<'static>,
    probe_buf: Vec<u8>,
}

struct KiroKasStreamExtractor {
    outer_dec: zstd::stream::raw::Decoder<'static>,
    inner_dec: Option<zstd::stream::raw::Decoder<'static>>,
    candidates: Vec<InnerCandidate>,
    inner_tar: TarPackageJsonScanner,
    magic_tail: Vec<u8>,
    outer_out: Vec<u8>,
    inner_out: Vec<u8>,
    found_version: Option<String>,
}

fn probe_looks_like_tar(buf: &[u8]) -> bool {
    buf.len() >= 265
        && (&buf[257..262] == b"ustar"
            || buf.starts_with(b"././@PaxHeader")
            || buf.starts_with(b"node_modules/"))
}

impl KiroKasStreamExtractor {
    fn new() -> Result<Self, String> {
        let outer_dec = zstd::stream::raw::Decoder::new().map_err(|e| e.to_string())?;
        Ok(Self {
            outer_dec,
            inner_dec: None,
            candidates: Vec::new(),
            inner_tar: TarPackageJsonScanner::default(),
            magic_tail: Vec::new(),
            outer_out: vec![0u8; 64 * 1024],
            inner_out: vec![0u8; 64 * 1024],
            found_version: None,
        })
    }

    fn feed_compressed(&mut self, mut chunk: &[u8]) -> Result<Option<String>, String> {
        use zstd::stream::raw::Operation;
        if let Some(ref v) = self.found_version {
            return Ok(Some(v.clone()));
        }
        while !chunk.is_empty() {
            let status = self
                .outer_dec
                .run_on_buffers(chunk, &mut self.outer_out)
                .map_err(|e| format!("outer zstd decode: {e}"))?;
            chunk = &chunk[status.bytes_read..];
            if status.bytes_written > 0 {
                let produced = self.outer_out[..status.bytes_written].to_vec();
                if let Some(version) = self.feed_outer_decompressed(&produced) {
                    self.found_version = Some(version.clone());
                    return Ok(Some(version));
                }
            }
            if status.bytes_read == 0 && status.bytes_written == 0 {
                break;
            }
        }
        Ok(self.found_version.clone())
    }

    fn feed_candidate(
        cand: &mut InnerCandidate,
        mut input: &[u8],
        scratch: &mut [u8],
    ) -> Result<Option<usize>, ()> {
        use zstd::stream::raw::Operation;
        let total = input.len();
        while !input.is_empty() {
            let status = cand.dec.run_on_buffers(input, scratch).map_err(|_| ())?;
            input = &input[status.bytes_read..];
            if status.bytes_written > 0 {
                cand.probe_buf.extend_from_slice(&scratch[..status.bytes_written]);
                if cand.probe_buf.len() >= 265 {
                    if probe_looks_like_tar(&cand.probe_buf) {
                        return Ok(Some(total - input.len()));
                    } else {
                        return Err(());
                    }
                }
            }
            if status.bytes_read == 0 && status.bytes_written == 0 {
                break;
            }
        }
        Ok(None)
    }

    fn feed_outer_decompressed(&mut self, data: &[u8]) -> Option<String> {
        if self.inner_dec.is_some() {
            return self.feed_inner(data);
        }

        // Advance existing unconfirmed candidates across chunk boundaries so a
        // zstd block split across 64 KiB chunks is never dropped.
        let mut next_candidates = Vec::with_capacity(self.candidates.len());
        let mut confirmed: Option<(InnerCandidate, usize)> = None;
        for mut cand in std::mem::take(&mut self.candidates) {
            match Self::feed_candidate(&mut cand, data, &mut self.inner_out) {
                Ok(Some(consumed)) => {
                    confirmed = Some((cand, consumed));
                    break;
                }
                Ok(None) => next_candidates.push(cand),
                Err(()) => {}
            }
        }
        if let Some((cand, consumed)) = confirmed {
            self.candidates.clear();
            self.magic_tail.clear();
            self.inner_tar = TarPackageJsonScanner::default();
            if let Some(version) = self.inner_tar.feed(&cand.probe_buf) {
                return Some(version);
            }
            self.inner_dec = Some(cand.dec);
            return self.feed_inner(&data[consumed..]);
        }
        self.candidates = next_candidates;

        // Scan for any new ZSTD_MAGIC starting in the 3-byte overlap + current chunk.
        let tail_len = self.magic_tail.len();
        let mut combined = Vec::with_capacity(tail_len + data.len());
        combined.extend_from_slice(&self.magic_tail);
        combined.extend_from_slice(data);

        let mut search_from = 0usize;
        while search_from + 4 <= combined.len() {
            let Some(rel) = combined[search_from..]
                .windows(4)
                .position(|w| w == ZSTD_MAGIC)
            else {
                break;
            };
            let pos = search_from + rel;
            search_from = pos + 1;
            if let Ok(dec) = zstd::stream::raw::Decoder::new() {
                let mut cand = InnerCandidate {
                    dec,
                    probe_buf: Vec::new(),
                };
                match Self::feed_candidate(&mut cand, &combined[pos..], &mut self.inner_out) {
                    Ok(Some(consumed)) => {
                        self.candidates.clear();
                        self.magic_tail.clear();
                        self.inner_tar = TarPackageJsonScanner::default();
                        if let Some(version) = self.inner_tar.feed(&cand.probe_buf) {
                            return Some(version);
                        }
                        self.inner_dec = Some(cand.dec);
                        return self.feed_inner(&combined[pos + consumed..]);
                    }
                    Ok(None) => {
                        if self.candidates.len() < 8 {
                            self.candidates.push(cand);
                        }
                    }
                    Err(()) => {}
                }
            }
        }

        let keep = combined.len().min(3);
        self.magic_tail = combined[combined.len() - keep..].to_vec();
        None
    }

    fn feed_inner(&mut self, mut data: &[u8]) -> Option<String> {
        use zstd::stream::raw::Operation;
        let dec = self.inner_dec.as_mut()?;
        while !data.is_empty() {
            match dec.run_on_buffers(data, &mut self.inner_out) {
                Ok(status) => {
                    data = &data[status.bytes_read..];
                    if status.bytes_written > 0 {
                        if let Some(version) =
                            self.inner_tar.feed(&self.inner_out[..status.bytes_written])
                        {
                            return Some(version);
                        }
                    }
                    if status.bytes_read == 0 && status.bytes_written == 0 {
                        break;
                    }
                }
                Err(_) => {
                    self.inner_dec = None;
                    return None;
                }
            }
        }
        None
    }
}

pub fn resolve_kas_package_url(manifest: &serde_json::Value, manifest_url: &str) -> Option<String> {
    let packages = manifest.get("packages")?.as_array()?;
    let chosen = packages
        .iter()
        .filter(|pkg| {
            pkg.get("fileType").and_then(serde_json::Value::as_str) == Some("tarZst")
                && pkg.get("download").and_then(serde_json::Value::as_str).is_some()
        })
        .min_by_key(|pkg| {
            pkg.get("size")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(u64::MAX)
        })?;
    let download = chosen.get("download")?.as_str()?;
    if download.starts_with("http://") || download.starts_with("https://") {
        return Some(download.to_string());
    }
    let trimmed = manifest_url.trim_end_matches('/');
    let base = trimmed
        .strip_suffix("/latest/manifest.json")
        .or_else(|| trimmed.rsplit_once('/').map(|(parent, _)| parent))?;
    Some(format!("{base}/{}", download.trim_start_matches('/')))
}

pub async fn fetch_kas_version_from_tar_zst_url(
    client: &reqwest::Client,
    url: &str,
) -> Result<Option<String>, String> {
    use futures::StreamExt;
    let resp = client
        .get(url)
        .timeout(std::time::Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let mut extractor = KiroKasStreamExtractor::new()?;
    let mut stream = resp.bytes_stream();
    let mut total_downloaded = 0usize;
    while let Some(item) = stream.next().await {
        let chunk = item.map_err(|e| e.to_string())?;
        total_downloaded = total_downloaded.saturating_add(chunk.len());
        if let Some(version) = extractor.feed_compressed(&chunk)? {
            return Ok(Some(version));
        }
        if total_downloaded >= MAX_KAS_PACKAGE_DOWNLOAD_BYTES {
            break;
        }
    }
    Ok(None)
}

pub async fn refresh_kiro_cli_version_from_url(
    client: &reqwest::Client,
    url: &str,
) -> Result<bool, String> {
    let resp = client
        .get(url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| e.to_string())?;
    let version = body
        .get("version")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "manifest missing version".to_string())?;
    let cli_changed = update_kiro_cli_version(version);
    let mut kas_changed = false;
    if let Some(kas) = body
        .get("kasVersion")
        .or_else(|| body.get("kas_version"))
        .and_then(serde_json::Value::as_str)
    {
        kas_changed = update_kiro_kas_version(kas);
    } else if cli_changed {
        if let Some(pkg_url) = resolve_kas_package_url(&body, url) {
            if let Ok(Some(kas)) = fetch_kas_version_from_tar_zst_url(client, &pkg_url).await {
                kas_changed = update_kiro_kas_version(&kas);
            }
        }
    }
    Ok(cli_changed || kas_changed)
}

pub async fn refresh_kiro_cli_version(client: &reqwest::Client) -> Result<bool, String> {
    refresh_kiro_cli_version_from_url(client, KIRO_CLI_MANIFEST_URL).await
}

fn kiro_cli_os_tag() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

pub fn kiro_cli_user_agent() -> String {
    let version = current_kiro_cli_version();
    let kas = current_kiro_kas_version();
    format!(
        "KiroCLI/{version} KAS/{kas} os/{} md/appVersion-{version} app/AmazonQ-For-CLI acp-client/kiro-cli",
        kiro_cli_os_tag()
    )
}

pub fn kiro_cli_amz_user_agent() -> String {
    let version = current_kiro_cli_version();
    let kas = current_kiro_kas_version();
    format!("KiroCLI/{version} KAS/{kas} acp-client/kiro-cli")
}

pub fn kiro_token_type_header(mode: KiroAuthMode) -> Option<&'static str> {
    match mode {
        KiroAuthMode::Social => None,
        KiroAuthMode::Idc => Some("SSO_OIDC"),
        KiroAuthMode::ExternalIdp => Some("EXTERNAL_IDP"),
        KiroAuthMode::ApiKey => Some("API_KEY"),
        KiroAuthMode::MachineToken => Some("KIRO_MACHINE_TOKEN"),
    }
}

/// Requesting a model outside this set makes the upstream answer
/// INVALID_MODEL_ID, which cools down the whole Kiro auth and breaks the
/// supported models too. Expand only for paid accounts.
use mahoquot_registry::{embedded_snapshot, ProviderContribution, ProviderId, RegistrySnapshot};

pub fn provider_id() -> ProviderId {
    ProviderId::kiro()
}

pub fn contribution(snapshot: &RegistrySnapshot) -> ProviderContribution {
    snapshot.contribution_for_provider(&provider_id())
}

pub fn default_contribution() -> ProviderContribution {
    contribution(embedded_snapshot())
}

pub fn supported_models(snapshot: &RegistrySnapshot) -> Vec<String> {
    snapshot.supported_model_ids_for_provider(&provider_id())
}

pub fn is_kiro_model_in_snapshot(snapshot: &RegistrySnapshot, model: &str) -> bool {
    snapshot.provider_supports_model(&provider_id(), model)
}

pub fn is_kiro_model(model: &str) -> bool {
    is_kiro_model_in_snapshot(embedded_snapshot(), model)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KiroAuthMode {
    #[default]
    #[serde(alias = "Social", alias = "SOCIAL")]
    Social,
    #[serde(alias = "IdC", alias = "IDC", alias = "sso_oidc", alias = "SSO_OIDC")]
    Idc,
    #[serde(alias = "ExternalIdp", alias = "External_IDP", alias = "EXTERNAL_IDP")]
    ExternalIdp,
    #[serde(alias = "ApiKey", alias = "API_KEY")]
    ApiKey,
    #[serde(alias = "MachineToken", alias = "KIRO_MACHINE_TOKEN")]
    MachineToken,
}

fn render_region(template: &str, region: &str) -> String {
    let region = if region.is_empty() {
        KIRO_DEFAULT_REGION
    } else {
        region
    };
    template.replace("{region}", region)
}

pub fn kiro_refresh_url(mode: KiroAuthMode, region: &str) -> String {
    let template = match mode {
        KiroAuthMode::Social => KIRO_SOCIAL_REFRESH_TEMPLATE,
        KiroAuthMode::Idc
        | KiroAuthMode::ExternalIdp
        | KiroAuthMode::ApiKey
        | KiroAuthMode::MachineToken => KIRO_IDC_REFRESH_TEMPLATE,
    };
    render_region(template, region)
}

pub fn kiro_generate_url(upstream_base: Option<&str>, region: &str) -> String {
    let base = match upstream_base {
        Some(base) => base.trim_end_matches('/').to_string(),
        None => render_region(KIRO_API_HOST_TEMPLATE, region),
    };
    format!("{base}{KIRO_GENERATE_PATH}")
}

pub fn kiro_responses_url(upstream_base: Option<&str>, region: &str) -> String {
    let base = match upstream_base {
        Some(base) => base.trim_end_matches('/').to_string(),
        None => render_region(KIRO_API_HOST_TEMPLATE, region),
    };
    format!("{base}{KIRO_RESPONSES_PATH}")
}

pub fn kiro_feature_config_url(upstream_base: Option<&str>, region: &str) -> String {
    let base = match upstream_base {
        Some(base) => base.trim_end_matches('/').to_string(),
        None => render_region(KIRO_API_HOST_TEMPLATE, region),
    };
    format!(
        "{base}{KIRO_FEATURE_CONFIG_PATH}?origin={KIRO_CLI_ORIGIN}&version={}",
        current_kiro_cli_version()
    )
}

pub fn kiro_usage_url(
    upstream_base: Option<&str>,
    region: &str,
    profile_arn: Option<&str>,
) -> String {
    if let Some(base) = upstream_base {
        return format!("{}/getUsageLimits", base.trim_end_matches('/'));
    }
    let base = render_region(KIRO_MANAGEMENT_HOST_TEMPLATE, region);
    let mut url = format!(
        "{base}{KIRO_USAGE_LIMITS_PATH}?origin={KIRO_CLI_ORIGIN}&resourceType=AGENTIC_REQUEST"
    );
    if let Some(arn) = profile_arn.map(str::trim).filter(|arn| !arn.is_empty()) {
        url.push_str("&profileArn=");
        for byte in arn.bytes() {
            if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
                url.push(byte as char);
            } else {
                url.push_str(&format!("%{byte:02X}"));
            }
        }
    }
    url
}

#[derive(Clone, Default, serde::Deserialize, serde::Serialize)]
pub struct KiroAccount {
    #[serde(default)]
    pub identity_slug: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default, alias = "kiroApiKey", alias = "api_key")]
    pub api_key: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub expired: String,
    #[serde(default)]
    pub region: String,
    #[serde(default, alias = "authMethod", alias = "auth_method")]
    pub auth_mode: KiroAuthMode,
    /// Present only on SSO OIDC credentials, which refresh as an OAuth client.
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub client_secret: String,
    #[serde(default, rename = "profileArn", alias = "profile_arn")]
    pub profile_arn: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(rename = "type")]
    pub r#type: String,
}

impl KiroAccount {
    pub fn effective_region(&self) -> &str {
        if self.region.is_empty() {
            KIRO_DEFAULT_REGION
        } else {
            &self.region
        }
    }

    pub fn effective_access_token(&self) -> &str {
        if self.auth_mode == KiroAuthMode::ApiKey && !self.api_key.is_empty() {
            &self.api_key
        } else if !self.access_token.is_empty() {
            &self.access_token
        } else {
            &self.api_key
        }
    }

    pub fn is_static_token(&self) -> bool {
        matches!(
            self.auth_mode,
            KiroAuthMode::ApiKey | KiroAuthMode::MachineToken
        ) || (self.auth_mode == KiroAuthMode::ExternalIdp && self.refresh_token.is_empty())
    }
}

impl std::fmt::Debug for KiroAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KiroAccount")
            .field("identity_slug", &self.identity_slug)
            .field("access_token", &"[REDACTED]")
            .field("api_key", &"[REDACTED]")
            .field("refresh_token", &"[REDACTED]")
            .field("email", &self.email)
            .field("expired", &self.expired)
            .field("region", &self.region)
            .field("auth_mode", &self.auth_mode)
            .field("client_id", &self.client_id)
            .field("client_secret", &"[REDACTED]")
            .field("profile_arn", &self.profile_arn)
            .field("disabled", &self.disabled)
            .field("type", &self.r#type)
            .finish()
    }
}

pub fn list_kiro_auth_files(dir: &Path) -> Result<Vec<PathBuf>, LoadError> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("kiro-") && n.ends_with(".json"))
        })
        .collect();
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_takes_kiro_files_and_leaves_other_providers_alone() {
        let dir = std::env::temp_dir().join(format!("qp-kiro-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        for name in [
            "kiro-a.json",
            "kiro-b.json",
            "codex-a.json",
            "zcode-a.json",
            "kiro-old.bak",
        ] {
            std::fs::write(dir.join(name), "{}").expect("write");
        }

        let found = list_kiro_auth_files(&dir).expect("listing");
        let names: Vec<String> = found
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(String::from))
            .collect();

        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(names, vec!["kiro-a.json", "kiro-b.json"]);
    }

    #[test]
    fn credential_parses_defaults_to_social_and_redacts_secrets() {
        let account: KiroAccount = serde_json::from_str(
            r#"{"access_token":"secret-access","refresh_token":"secret-refresh",
                "email":"u@example.com","expired":"2027-01-01T00:00:00Z",
                "client_secret":"secret-client","type":"kiro"}"#,
        )
        .expect("deserialize");

        assert_eq!(account.auth_mode, KiroAuthMode::Social);
        assert_eq!(account.effective_region(), KIRO_DEFAULT_REGION);

        let rendered = format!("{account:?}");
        assert!(!rendered.contains("secret-access"));
        assert!(!rendered.contains("secret-refresh"));
        assert!(!rendered.contains("secret-client"));
    }

    #[test]
    fn refresh_url_switches_host_per_auth_mode_and_region() {
        assert_eq!(
            kiro_refresh_url(KiroAuthMode::Social, "us-east-1"),
            "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken"
        );
        assert_eq!(
            kiro_refresh_url(KiroAuthMode::Idc, "eu-central-1"),
            "https://oidc.eu-central-1.amazonaws.com/token"
        );
        assert_eq!(
            kiro_refresh_url(KiroAuthMode::Social, ""),
            "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken"
        );
    }

    #[test]
    fn generate_url_uses_region_host_or_override() {
        assert_eq!(
            kiro_generate_url(None, "eu-central-1"),
            "https://runtime.eu-central-1.kiro.dev/generateAssistantResponse"
        );
        assert_eq!(
            kiro_generate_url(Some("http://127.0.0.1:18892/"), "us-east-1"),
            "http://127.0.0.1:18892/generateAssistantResponse"
        );
    }

    #[test]
    fn model_matcher_accepts_known_models_only() {
        assert!(is_kiro_model("claude-opus-5.5"));
        // Retired ids and the auto shim are outside the closed catalog now.
        assert!(!is_kiro_model("claude-sonnet-4.6"));
        assert!(!is_kiro_model("claude-opus-4.6"));
        assert!(!is_kiro_model("claude-haiku-4.5"));
        assert!(!is_kiro_model("auto"));
        assert!(!is_kiro_model("claude-opus-4-5-20251101"));
        // Date-suffixed ids are rejected upstream with INVALID_MODEL_ID.
        assert!(!is_kiro_model("claude-haiku-4-5-20251001"));
    }
}

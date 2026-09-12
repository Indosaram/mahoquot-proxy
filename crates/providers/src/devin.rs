//! Devin provider credentials.
//!
//! Contract per `.omo/plans/devin-provider-integration.md` §6.1 and the pinned
//! upstream reference (`Arborsm/dsh-plugin-devin-bridge` @
//! ced035dd2f43b2dbf37a25c8a47b8ae1c1cc47f4, `src/adapter/credentials.ts`):
//!
//! - Devin CLI stores a session token in `credentials.toml` as
//!   `windsurf_api_key`, with an optional `api_server_url`. There is NO
//!   refresh/OAuth endpoint for this token; expiry handling is
//!   re-authentication only. This module deliberately invents neither.
//! - Upstream auth is the LITERAL header value
//!   `Basic <token>-<token>` (plain concatenation of the same token twice).
//!   It is NOT base64(`user:pass`); `reqwest::header::basic_auth` must never
//!   be used for Devin.
//! - Normalized stored form (gateway-owned atomic writer applies it):
//!   `{"type":"devin","identity_slug":..,"label":..,"email":..,
//!     "access_token":..,"api_server_url":..,"disabled":bool}`.
//!
//! This module is pure parsing/validation/path logic. Persistence, HTTP and
//! blocking-I/O offloading belong to the gateway.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Serialized `type` discriminator of a Devin account record.
pub const DEVIN_TYPE: &str = "devin";
/// Official upstream API server used when the CLI file omits `api_server_url`.
pub const DEVIN_DEFAULT_API_SERVER_URL: &str = "https://server.codeium.com";
/// File name of the Devin CLI credential file inside its data directory.
pub const DEVIN_CREDENTIALS_FILE: &str = "credentials.toml";
/// Environment variable name for explicit credentials path override.
pub const DEVIN_CREDENTIALS_PATH_ENV: &str = "DEVIN_CREDENTIALS_PATH";
/// CLI TOML field holding the session token (kept for import fidelity).
pub const DEVIN_CLI_TOKEN_KEY: &str = "windsurf_api_key";

#[derive(Debug, thiserror::Error)]
pub enum DevinCredentialsError {
    #[error("reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path}: {msg}")]
    Parse { path: PathBuf, msg: String },
    #[error("invalid session token: {reason}")]
    InvalidToken { reason: String },
    #[error("invalid identity: {reason}")]
    InvalidIdentity { reason: String },
    #[error("invalid provider type {found:?}: must be {DEVIN_TYPE:?}")]
    InvalidProviderType { found: String },
    #[error("invalid api_server_url {url:?}: {reason}")]
    InvalidUrl { url: String, reason: String },
}

/// Normalized Devin account record.
///
/// The serialized form matches the gateway auth directory schema; `type` is
/// always `devin` and missing optional fields are defaulted on load.
#[derive(Clone, Serialize, Deserialize)]
pub struct DevinAccount {
    /// Fixed to `"devin"` on both directions.
    #[serde(rename = "type", default = "default_type")]
    pub provider_type: String,
    /// Stable, non-empty account identity (survives token replacement).
    #[serde(default)]
    pub identity_slug: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Devin CLI session token. Not a JWT; never refreshed programmatically.
    pub access_token: String,
    /// Base origin; defaults to [`DEVIN_DEFAULT_API_SERVER_URL`].
    #[serde(default = "default_api_server_url", alias = "apiServerUrl")]
    pub api_server_url: String,
    #[serde(default)]
    pub disabled: bool,
}

fn default_type() -> String {
    DEVIN_TYPE.to_string()
}

fn default_api_server_url() -> String {
    DEVIN_DEFAULT_API_SERVER_URL.to_string()
}

impl std::fmt::Debug for DevinAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevinAccount")
            .field("provider_type", &self.provider_type)
            .field("identity_slug", &self.identity_slug)
            .field("label", &self.label)
            .field("email", &self.email)
            .field("access_token", &"[REDACTED]")
            .field(
                "api_server_url",
                &sanitize_url_for_debug(&self.api_server_url),
            )
            .field("disabled", &self.disabled)
            .finish()
    }
}

/// Official Devin CLI `credentials.toml` shape. Extra keys are ignored so the
/// CLI may evolve without breaking imports; unknown fields are NOT copied.
#[derive(Deserialize)]
pub struct DevinCliCredentials {
    /// Session token field of the Devin CLI, named `windsurf_api_key` for
    /// import fidelity with the official CLI.
    #[serde(rename = "windsurf_api_key", alias = "windsurfApiKey")]
    pub windsurf_api_key: String,
    #[serde(rename = "api_server_url", alias = "apiServerUrl", default)]
    pub api_server_url: Option<String>,
}

impl std::fmt::Debug for DevinCliCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DevinCliCredentials")
            .field("windsurf_api_key", &"[REDACTED]")
            .field(
                "api_server_url",
                &self.api_server_url.as_deref().map(sanitize_url_for_debug),
            )
            .finish()
    }
}

impl DevinAccount {
    /// Same account with a fresh token; identity, label, URL and flags are
    /// preserved so a credential swap never changes account identity.
    pub fn replace_token(&self, access_token: impl Into<String>) -> Self {
        let mut next = self.clone();
        next.access_token = access_token.into();
        next
    }

    /// Builder-style overrides used by tests and gateway import flows.
    pub fn with_identity_slug(mut self, slug: impl Into<String>) -> Self {
        self.identity_slug = slug.into();
        self
    }

    pub fn with_api_server_url(mut self, url: impl Into<String>) -> Self {
        self.api_server_url = url.into();
        self
    }

    /// Normalizes and validates at the trust boundary: identity must be
    /// non-empty and a safe filename slug, the token injection-safe and
    /// unmutated, and the URL a credentials-free http(s) origin.
    pub fn validate(self) -> Result<DevinAccount, DevinCredentialsError> {
        let identity_slug = validate_identity_slug(&self.identity_slug)?;
        if self.access_token.is_empty() {
            return Err(DevinCredentialsError::InvalidToken {
                reason: "session token must be non-empty".to_string(),
            });
        }
        if self
            .access_token
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(DevinCredentialsError::InvalidToken {
                reason: "session token must not contain whitespace or control characters"
                    .to_string(),
            });
        }
        if self.access_token.len() > 4096 {
            return Err(DevinCredentialsError::InvalidToken {
                reason: "session token exceeds 4096 bytes".to_string(),
            });
        }
        let access_token = self.access_token;
        let api_server_url = validate_api_server_url(&self.api_server_url)?;
        let provider_type = if self.provider_type.is_empty() {
            DEVIN_TYPE.to_string()
        } else if self.provider_type == DEVIN_TYPE {
            self.provider_type
        } else {
            return Err(DevinCredentialsError::InvalidProviderType {
                found: self.provider_type,
            });
        };
        Ok(DevinAccount {
            provider_type,
            identity_slug,
            label: self
                .label
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty()),
            email: self
                .email
                .map(|e| e.trim().to_string())
                .filter(|e| !e.is_empty()),
            access_token,
            api_server_url,
            disabled: self.disabled,
        })
    }

    /// Normalized, validated account from raw CLI record plus explicit identity.
    pub fn from_cli(
        cli: DevinCliCredentials,
        identity_slug: impl Into<String>,
        label: Option<String>,
    ) -> Result<DevinAccount, DevinCredentialsError> {
        DevinAccount {
            provider_type: DEVIN_TYPE.to_string(),
            identity_slug: identity_slug.into(),
            label,
            email: None,
            access_token: cli.windsurf_api_key,
            api_server_url: cli
                .api_server_url
                .filter(|u| !u.trim().is_empty())
                .unwrap_or_else(|| DEVIN_DEFAULT_API_SERVER_URL.to_string()),
            disabled: false,
        }
        .validate()
    }

    /// Upstream Connect auth header pair: `Authorization: Basic <token>-<token>`
    /// — the literal value, never base64-encoded.
    pub fn authorization_header(&self) -> (String, String) {
        (
            "Authorization".to_string(),
            format!("Basic {}-{}", self.access_token, self.access_token),
        )
    }

    pub fn identity_slug(&self) -> &str {
        &self.identity_slug
    }

    pub fn label(&self) -> Option<&str> {
        self.label.as_deref()
    }

    pub fn email(&self) -> Option<&str> {
        self.email.as_deref()
    }

    pub fn access_token_secret(&self) -> &str {
        &self.access_token
    }

    pub fn api_server_url(&self) -> &str {
        &self.api_server_url
    }

    pub fn provider_type(&self) -> &str {
        &self.provider_type
    }

    pub fn disabled(&self) -> bool {
        self.disabled
    }
}

/// Validates an account identity slug. Identity is used as a filename
/// component by the gateway (`devin-{slug}.json`).
///
/// Enforces a safe slug without path separators, path traversal, control
/// characters, or whitespace, while preserving distinct existing identities
/// (e.g. `work` and `devin-work` are never collapsed).
pub fn validate_identity_slug(slug: &str) -> Result<String, DevinCredentialsError> {
    let rejected = |reason: &str| DevinCredentialsError::InvalidIdentity {
        reason: reason.to_string(),
    };
    if slug.is_empty() {
        return Err(rejected("identity_slug must be non-empty"));
    }
    if slug != slug.trim() || slug.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(rejected(
            "identity_slug must not contain whitespace or control characters",
        ));
    }
    if slug == "." || slug == ".." || slug.contains("..") {
        return Err(rejected(
            "identity_slug must not contain path traversal segments",
        ));
    }
    if slug.contains('/') || slug.contains('\\') || slug.contains(':') {
        return Err(rejected("identity_slug must not contain path separators"));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(rejected(
            "identity_slug must contain only ASCII alphanumeric, '-', '_', or '.' characters",
        ));
    }
    Ok(slug.to_string())
}

/// Sanitizes a URL for safe inclusion in logs, Debug output, and error messages.
/// Produces a fixed safe URL description that never echoes credentials,
/// query parameters, or arbitrary path tokens.
pub fn sanitize_url_for_debug(url: &str) -> String {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return "<invalid-url>".to_string();
    };

    let scheme = parsed.scheme();
    let has_userinfo = !parsed.username().is_empty() || parsed.password().is_some();
    let userinfo_str = if has_userinfo { "[REDACTED]@" } else { "" };
    let host = parsed.host_str().unwrap_or("");
    let port_str = match parsed.port() {
        Some(p) => format!(":{p}"),
        None => String::new(),
    };

    // Safe base-path handling: root "/" is preserved; non-root paths are
    // redacted so tokens embedded in arbitrary paths cannot leak into logs/Debug.
    let path = parsed.path();
    let path_str = if path == "/" {
        if url.ends_with('/') {
            "/"
        } else {
            ""
        }
    } else if path.is_empty() {
        ""
    } else {
        "/[REDACTED]"
    };

    let query_str = if parsed.query().is_some() {
        "?[REDACTED]"
    } else {
        ""
    };

    let fragment_str = if parsed.fragment().is_some() {
        "#[REDACTED]"
    } else {
        ""
    };

    format!("{scheme}://{userinfo_str}{host}{port_str}{path_str}{query_str}{fragment_str}")
}

/// Validates an API server origin URL per plan §6.1.
///
/// Requires an absolute HTTP or HTTPS URL with a valid host and port.
/// Credentials, query parameters, fragment identifiers, and path traversal
/// segments are strictly rejected at the trust boundary.
pub fn validate_api_server_url(url: &str) -> Result<String, DevinCredentialsError> {
    let sanitized_url = sanitize_url_for_debug(url);
    let rejected = |reason: &str| DevinCredentialsError::InvalidUrl {
        url: sanitized_url.clone(),
        reason: reason.to_string(),
    };

    // Edge whitespace or control characters are rejected directly on the original string
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(rejected(
            "url must not contain whitespace or control characters",
        ));
    }

    // Must start with lowercase http:// or https:// and not have an empty host (///)
    if (!url.starts_with("https://") && !url.starts_with("http://"))
        || url.starts_with("https:///")
        || url.starts_with("http:///")
    {
        return Err(rejected(
            if url.starts_with("https:///") || url.starts_with("http:///") {
                "missing host"
            } else {
                "scheme must be http or https"
            },
        ));
    }

    let parsed = reqwest::Url::parse(url).map_err(|e| rejected(&format!("invalid URL: {e}")))?;

    // Scheme must be http or https
    if parsed.scheme() != "https" && parsed.scheme() != "http" {
        return Err(rejected("scheme must be http or https"));
    }

    // Reject credentials
    if !parsed.username().is_empty() || parsed.password().is_some() || url.contains('@') {
        return Err(rejected("credentials in URL are not permitted"));
    }

    // Reject query and fragment
    if parsed.query().is_some() || url.contains('?') {
        return Err(rejected(
            "query parameters are not permitted in api_server_url",
        ));
    }
    if parsed.fragment().is_some() || url.contains('#') {
        return Err(rejected("fragments are not permitted in api_server_url"));
    }

    // Host check
    let Some(host) = parsed.host_str() else {
        return Err(rejected("missing host"));
    };
    if host.is_empty() {
        return Err(rejected("missing host"));
    }

    // Port check: if explicitly specified, must be non-zero
    if let Some(port) = parsed.port() {
        if port == 0 {
            return Err(rejected("port must be non-zero"));
        }
    }

    // Base-path semantics:
    // Only root ("/" or empty) or safe normalized path prefixes are allowed.
    // Traversal segments (/../ or /.) and empty segments (//) are rejected.
    let path = parsed.path();
    if url.contains("/..") || url.contains("../") || url.contains("/./") || url.ends_with("/.") {
        return Err(rejected(
            "path traversal segments are not permitted in api_server_url",
        ));
    }
    if path.contains("//") {
        return Err(rejected(
            "empty path segments are not permitted in api_server_url",
        ));
    }

    Ok(url.to_string())
}

/// Reads and normalizes a Devin CLI `credentials.toml` file into an account.
///
/// The original bytes are only ever read, never written or removed. The
/// identity slug is derived from the file stem (e.g. `credentials.toml` ->
/// `credentials`); the gateway import flow passes its own stable identity and
/// should use [`parse_cli_credentials`] + [`DevinAccount::from_cli`] instead.
pub fn load_devin_account(path: &Path) -> Result<DevinAccount, DevinCredentialsError> {
    let bytes = std::fs::read(path).map_err(|source| DevinCredentialsError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let cli = parse_cli_credentials(&bytes, path)?;
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| DEVIN_TYPE.to_string());
    DevinAccount::from_cli(cli, stem, None)
}

/// Sanitizes a TOML error for safe inclusion in error messages.
///
/// Uses a fixed safe description plus optional numeric span to guarantee
/// that raw source messages, secret values, and unquoted input tokens never leak.
pub fn sanitize_toml_error(err: &toml::de::Error) -> String {
    if let Some(span) = err.span() {
        format!("invalid TOML at byte range {}..{}", span.start, span.end)
    } else {
        "invalid TOML".to_string()
    }
}

/// Parses official CLI credential bytes with a real TOML parser.
pub fn parse_cli_credentials(
    bytes: &[u8],
    path: &Path,
) -> Result<DevinCliCredentials, DevinCredentialsError> {
    let parse_error = |msg: String| DevinCredentialsError::Parse {
        path: path.to_path_buf(),
        msg,
    };
    let text =
        std::str::from_utf8(bytes).map_err(|e| parse_error(format!("not valid UTF-8: {e}")))?;
    let cli: DevinCliCredentials =
        toml::from_str(text).map_err(|e| parse_error(sanitize_toml_error(&e)))?;
    if cli.windsurf_api_key.trim().is_empty() {
        return Err(parse_error(format!(
            "`{DEVIN_CLI_TOKEN_KEY}` must be non-empty"
        )));
    }
    Ok(cli)
}

/// Resolves the Devin CLI credential file path without touching process
/// environment or filesystem state. Priority: explicit path, then
/// `$XDG_DATA_HOME/devin/credentials.toml`, then
/// `<home>/.local/share/devin/credentials.toml`. Empty values count as unset.
pub fn resolve_credentials_path(
    explicit: Option<&Path>,
    xdg_data_home: Option<&Path>,
    home: Option<&Path>,
) -> PathBuf {
    if let Some(explicit) = explicit {
        if !explicit.as_os_str().is_empty() {
            return explicit.to_path_buf();
        }
    }
    if let Some(xdg) = xdg_data_home {
        if !xdg.as_os_str().is_empty() {
            return xdg.join(DEVIN_TYPE).join(DEVIN_CREDENTIALS_FILE);
        }
    }
    home.unwrap_or_else(|| Path::new(""))
        .join(".local/share")
        .join(DEVIN_TYPE)
        .join(DEVIN_CREDENTIALS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_with_tab_is_rejected() {
        let err = DevinAccount {
            provider_type: DEVIN_TYPE.to_string(),
            identity_slug: "x".to_string(),
            label: None,
            email: None,
            access_token: "a\tb".to_string(),
            api_server_url: DEVIN_DEFAULT_API_SERVER_URL.to_string(),
            disabled: false,
        }
        .validate();
        assert!(matches!(
            err,
            Err(DevinCredentialsError::InvalidToken { .. })
        ));
    }

    #[test]
    fn label_is_trimmed_and_empty_becomes_none() {
        let acct = DevinAccount {
            provider_type: DEVIN_TYPE.to_string(),
            identity_slug: "x".to_string(),
            label: Some("  ".to_string()),
            email: Some(" a@b.c ".to_string()),
            access_token: "tok".to_string(),
            api_server_url: DEVIN_DEFAULT_API_SERVER_URL.to_string(),
            disabled: false,
        }
        .validate()
        .expect("valid");
        assert_eq!(acct.label(), None);
        assert_eq!(acct.email(), Some("a@b.c"));
    }

    #[test]
    fn sanitize_toml_error_does_not_leak_expected_sentinel() {
        let toml_str = "provider_type = \"devin\"\nidentity_slug = \"id\"\naccess_token = \"tok\"\napi_server_url = \"https://server.codeium.com\"\ndisabled = \"expected secret-sentinel\"\n";
        let err: Result<DevinAccount, _> = toml::from_str(toml_str);
        let de_err = err.unwrap_err();
        let sanitized = sanitize_toml_error(&de_err);
        assert!(
            !sanitized.contains("secret-sentinel"),
            "leaked: {sanitized}"
        );
    }
}

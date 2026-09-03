use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::management::settings::ScopedApiKey;
use crate::management::store::SettingsStore;

/// Who a request authenticated as.
///
/// A master key is an entry in `api-keys` (or the env/CLI override) and carries
/// full authority, including the control plane. A scoped key is a delegated
/// credential from `scoped-api-keys`: it may relay traffic within its allow
/// lists but must never reach management or admin surfaces.
#[derive(Debug, Clone)]
pub enum AuthIdentity {
    /// No inbound keys are configured, so the gateway is open.
    Unrestricted,
    Master,
    Scoped(Arc<ScopedApiKey>),
}

impl AuthIdentity {
    pub fn is_scoped(&self) -> bool {
        matches!(self, AuthIdentity::Scoped(_))
    }

    /// Only master (or an unrestricted gateway) may touch `/v0/management/*`
    /// and `/admin/*`.
    pub fn may_manage(&self) -> bool {
        !self.is_scoped()
    }

    pub fn scoped(&self) -> Option<&Arc<ScopedApiKey>> {
        match self {
            AuthIdentity::Scoped(key) => Some(key),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ApiKeys {
    overrides: Vec<String>,
    settings: Option<Arc<SettingsStore>>,
}

impl ApiKeys {
    pub fn new(keys: Vec<String>) -> Self {
        Self {
            overrides: keys,
            settings: None,
        }
    }

    pub fn from_env_value(raw: &str) -> Self {
        let keys = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect();
        Self::new(keys)
    }

    pub fn with_live_settings(settings: Arc<SettingsStore>, overrides: Self) -> Self {
        Self {
            overrides: overrides.overrides,
            settings: Some(settings),
        }
    }

    pub fn values(&self) -> &[String] {
        &self.overrides
    }

    pub fn is_empty(&self) -> bool {
        self.overrides.is_empty()
            && self
                .settings
                .as_ref()
                .map(|settings| settings.current().api_keys.is_empty())
                .unwrap_or(true)
    }

    pub fn accepts(&self, presented: &str) -> bool {
        self.overrides.iter().any(|key| key == presented)
            || self
                .settings
                .as_ref()
                .map(|settings| {
                    settings
                        .current()
                        .api_keys
                        .iter()
                        .any(|key| key == presented)
                })
                .unwrap_or(false)
    }

    /// Resolve the presented credential to an identity, or `None` when it is
    /// not accepted. Master keys are checked first so a scoped key can never
    /// shadow one.
    pub fn authorizes(&self, presented: Option<&str>) -> Option<AuthIdentity> {
        let settings = self.settings.as_ref().map(|settings| settings.current());
        let live_keys = settings
            .as_ref()
            .map(|settings| settings.api_keys.as_slice())
            .unwrap_or_default();
        let scoped_keys = settings
            .as_ref()
            .map(|settings| settings.scoped_api_keys.as_slice())
            .unwrap_or_default();

        // An entirely unconfigured gateway stays open, but only while no master
        // key is defined: minting a scoped key delegates quota for external callers
        // without locking the local console out of its unauthenticated local gateway.
        if self.overrides.is_empty() && live_keys.is_empty() {
            if let Some(presented) = presented {
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64)
                    .unwrap_or(0);
                let identifier = crate::request_history::stable_key_identifier(presented);
                if let Some(key) = scoped_keys
                    .iter()
                    .find(|key| key.key_identifier == identifier && key.is_usable_at(now_ms))
                {
                    return Some(AuthIdentity::Scoped(Arc::new(key.clone())));
                }
            }
            return Some(AuthIdentity::Master);
        }

        let presented = presented?;
        if self.overrides.iter().any(|accepted| accepted == presented)
            || live_keys.iter().any(|accepted| accepted == presented)
        {
            return Some(AuthIdentity::Master);
        }

        if scoped_keys.is_empty() {
            return None;
        }

        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(0);
        let identifier = crate::request_history::stable_key_identifier(presented);
        scoped_keys
            .iter()
            .find(|key| key.key_identifier == identifier && key.is_usable_at(now_ms))
            .map(|key| AuthIdentity::Scoped(Arc::new(key.clone())))
    }
}

/// Nesting rewrites the inner request URI, so under `/v0/management` this
/// middleware only sees the group-relative path. Axum preserves the untouched
/// URI in `OriginalUri`; consult it first and fall back to the request URI for
/// the merged (non-nested) routes.
fn request_targets_management(req: &Request) -> bool {
    let original = req
        .extensions()
        .get::<axum::extract::OriginalUri>()
        .map(|uri| uri.0.path());
    original.is_some_and(is_management_path) || is_management_path(req.uri().path())
}

/// Control-plane prefixes a scoped key may never reach.
fn is_management_path(path: &str) -> bool {
    const PREFIXES: [&str; 2] = ["/v0/management", "/admin"];
    PREFIXES.iter().any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

pub async fn require_api_key(
    State(keys): State<Arc<ApiKeys>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(identity) = keys.authorizes(extract_presented_key(&req)) else {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":{"message":"invalid api key","type":"invalid_request_error"}}"#,
        )
            .into_response();
    };

    if !identity.may_manage() && request_targets_management(&req) {
        return (
            StatusCode::FORBIDDEN,
            [(header::CONTENT_TYPE, "application/json")],
            r#"{"error":{"message":"scoped api keys cannot access management endpoints","type":"permission_error"}}"#,
        )
            .into_response();
    }

    // Downstream handlers read the identity instead of re-deriving it from the
    // header, so scope enforcement and auth share one decision.
    req.extensions_mut().insert(identity);
    next.run(req).await
}

pub fn presented_api_key(headers: &HeaderMap) -> Option<&str> {
    if let Some(auth_val) = headers.get(header::AUTHORIZATION) {
        if let Ok(auth_str) = auth_val.to_str() {
            if let Some(token) = auth_str
                .strip_prefix("Bearer ")
                .or_else(|| auth_str.strip_prefix("bearer "))
            {
                return Some(token.trim());
            }
        }
    }
    headers
        .get("x-api-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
}

fn extract_presented_key(req: &Request) -> Option<&str> {
    if let Some(auth_val) = req.headers().get(header::AUTHORIZATION) {
        if let Ok(auth_str) = auth_val.to_str() {
            if let Some(token) = auth_str
                .strip_prefix("Bearer ")
                .or_else(|| auth_str.strip_prefix("bearer "))
            {
                return Some(token.trim());
            }
        }
    }

    if let Some(api_key_val) = req.headers().get("x-api-key") {
        if let Ok(api_key_str) = api_key_val.to_str() {
            return Some(api_key_str.trim());
        }
    }

    if let Some(query) = req.uri().query() {
        for pair in query.split('&') {
            if let Some((k, v)) = pair.split_once('=') {
                if k == "key" {
                    return Some(v);
                }
            }
        }
    }

    None
}

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::management::store::SettingsStore;

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

    fn authorizes(&self, presented: Option<&str>) -> bool {
        let settings = self.settings.as_ref().map(|settings| settings.current());
        let live_keys = settings
            .as_ref()
            .map(|settings| settings.api_keys.as_slice())
            .unwrap_or_default();

        if self.overrides.is_empty() && live_keys.is_empty() {
            return true;
        }

        presented
            .map(|key| {
                self.overrides.iter().any(|accepted| accepted == key)
                    || live_keys.iter().any(|accepted| accepted == key)
            })
            .unwrap_or(false)
    }
}

pub async fn require_api_key(
    State(keys): State<Arc<ApiKeys>>,
    req: Request,
    next: Next,
) -> Response {
    if keys.authorizes(extract_presented_key(&req)) {
        return next.run(req).await;
    }

    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "application/json")],
        r#"{"error":{"message":"invalid api key","type":"invalid_request_error"}}"#,
    )
        .into_response()
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

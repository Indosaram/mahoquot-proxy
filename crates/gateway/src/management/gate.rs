use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

const MAX_FAILURES: u32 = 5;
const BAN_DURATION: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Default)]
pub struct ManagementAttempts {
    ips: HashMap<IpAddr, (u32, Option<Instant>, Instant)>,
}

impl ManagementAttempts {
    pub(crate) fn authenticate(
        &mut self,
        peer: IpAddr,
        allow_remote: bool,
        valid: bool,
        missing: bool,
        now: Instant,
    ) -> Result<(), (StatusCode, String)> {
        self.ips.retain(|_, (_, ban, activity)| {
            ban.is_some_and(|until| until > now)
                || now.duration_since(*activity) <= Duration::from_secs(2 * 3600)
        });
        if let Some((count, ban, _)) = self.ips.get_mut(&peer) {
            if let Some(until) = *ban {
                if now < until {
                    return Err((
                        StatusCode::FORBIDDEN,
                        format!(
                            "IP banned due to too many failed attempts. Try again in {}s",
                            until.duration_since(now).as_secs()
                        ),
                    ));
                }
                *count = 0;
                *ban = None;
            }
        }
        if peer != IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            && peer != IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
            && !allow_remote
        {
            return Err((StatusCode::FORBIDDEN, "remote management disabled".into()));
        }
        if valid {
            self.ips.remove(&peer);
            return Ok(());
        }
        let entry = self.ips.entry(peer).or_insert((0, None, now));
        entry.0 += 1;
        entry.2 = now;
        if entry.0 >= MAX_FAILURES {
            entry.0 = 0;
            entry.1 = Some(now + BAN_DURATION);
        }
        Err((
            StatusCode::UNAUTHORIZED,
            if missing {
                "missing management key"
            } else {
                "invalid management key"
            }
            .into(),
        ))
    }
}

fn presented_management_key(headers: &HeaderMap) -> &str {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let provided = match auth.split_once(' ') {
        Some((scheme, key)) if scheme.eq_ignore_ascii_case("bearer") => key,
        _ => auth,
    };
    if provided.is_empty() {
        headers
            .get("x-management-key")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
    } else {
        provided
    }
}

pub async fn require_management_key(
    settings: Option<&crate::management::store::SettingsStore>,
    attempts: &Mutex<ManagementAttempts>,
    req: Request,
    next: Next,
) -> Response {
    let configured = settings.map(|store| store.current());
    let secret = configured
        .as_ref()
        .map(|settings| settings.remote_management.secret_key.as_str())
        .unwrap_or("");
    let env_secret = std::env::var("MANAGEMENT_PASSWORD").unwrap_or_default();
    if secret.is_empty() && env_secret.is_empty() {
        return StatusCode::NOT_FOUND.into_response();
    }
    // Only the transport-authenticated peer is trusted, never Forwarded/X-Forwarded-For.
    let Some(peer) = req
        .extensions()
        .get::<axum::extract::ConnectInfo<SocketAddr>>()
        .map(|peer| peer.0.ip())
    else {
        return (
            StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({"error":"management peer unavailable"})),
        )
            .into_response();
    };
    let allow_remote = !env_secret.is_empty()
        || configured
            .as_ref()
            .is_some_and(|settings| settings.remote_management.allow_remote);
    let provided = presented_management_key(req.headers()).to_owned();
    let secret = secret.to_owned();
    let missing = provided.is_empty();
    let valid = if missing {
        false
    } else {
        match tokio::task::spawn_blocking(move || {
            if !env_secret.is_empty() && constant_time_equal(&env_secret, &provided) {
                return true;
            }
            if secret.starts_with("$2") {
                bcrypt::verify(&provided, &secret).unwrap_or(false)
            } else {
                !secret.is_empty() && constant_time_equal(&secret, &provided)
            }
        })
        .await
        {
            Ok(valid) => valid,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    };
    let verdict = match attempts.lock() {
        Ok(mut attempts) => {
            attempts.authenticate(peer, allow_remote, valid, missing, Instant::now())
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };
    match verdict {
        Ok(()) => next.run(req).await,
        Err((status, error)) => {
            (status, axum::Json(serde_json::json!({"error":error}))).into_response()
        }
    }
}

fn constant_time_equal(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0u8, |diff, (a, b)| diff | (a ^ b))
        == 0
}

/// Identity headers CLIProxyAPI stamps on every management response. Values
/// track the release this surface mirrors so a client cannot tell the two
/// proxies apart; `capability.rs` and `cp_routes.rs` pin the same version.
const CPA_VERSION: &str = "7.2.140";
const CPA_COMMIT: &str = "a7e3596b";
const CPA_BUILD_DATE: &str = "2026-08-22T14:51:09Z";
const CPA_SUPPORT_PLUGIN: &str = "1";

const EXPOSE_HEADERS: &str = "X-CPA-TRACE-ID, X-CPA-VERSION, X-CPA-COMMIT, X-CPA-BUILD-DATE, X-CPA-SUPPORT-PLUGIN, X-CPA-HOME-VERSION, X-CPA-HOME-BUILD-DATE, X-SERVER-VERSION, X-SERVER-BUILD-DATE, Location, Retry-After, X-Request-Id, OpenAI-Request-Id";

pub fn cpa_version() -> &'static str {
    CPA_VERSION
}

fn stamp(headers: &mut HeaderMap) {
    headers.insert("X-CPA-VERSION", HeaderValue::from_static(CPA_VERSION));
    headers.insert("X-CPA-COMMIT", HeaderValue::from_static(CPA_COMMIT));
    headers.insert("X-CPA-BUILD-DATE", HeaderValue::from_static(CPA_BUILD_DATE));
    headers.insert(
        "X-CPA-SUPPORT-PLUGIN",
        HeaderValue::from_static(CPA_SUPPORT_PLUGIN),
    );
    headers.insert(
        axum::http::header::ACCESS_CONTROL_EXPOSE_HEADERS,
        HeaderValue::from_static(EXPOSE_HEADERS),
    );
}

pub async fn stamp_management_response(req: Request, next: Next) -> Response {
    let mut response = next.run(req).await;
    stamp(response.headers_mut());
    response
}

pub async fn require_management_auth(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<crate::state::AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let attempts = state.api_keys.management_attempts();
    require_management_key(Some(&state.settings), &attempts, req, next).await
}

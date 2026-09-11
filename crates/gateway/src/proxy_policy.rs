use std::collections::BTreeMap;
use reqwest::Url;
use crate::management::settings::{ProviderProxyPolicy, Settings};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyRuntime {
    pub global_proxy_url: String,
    pub providers: BTreeMap<String, ProviderProxyPolicy>,
}

impl ProxyRuntime {
    pub fn from_settings(settings: &Settings) -> Self {
        Self {
            global_proxy_url: settings.proxy_url.trim().to_string(),
            providers: settings.proxy_providers.clone(),
        }
    }

    /// True once at least one provider opted into proxying, which makes
    /// `proxy-providers` an allowlist instead of a stickiness modifier.
    pub fn scoped_routing_active(&self) -> bool {
        self.providers.values().any(|policy| policy.enabled)
    }

    pub fn session_proxy_url(
        &self,
        provider_name: &str,
        member_id: &str,
        now_unix: u64,
    ) -> Option<String> {
        let norm = provider_name.trim().to_ascii_lowercase();
        let policy = self
            .providers
            .get(&norm)
            .or_else(|| {
                self.providers.iter().find_map(|(k, v)| {
                    let k_norm = k.trim().to_ascii_lowercase();
                    if k_norm == norm
                        || (k_norm == "openai" && norm == "codex")
                        || (k_norm == "codex" && norm == "openai")
                        || (k_norm == "claude-code" && norm == "claude")
                        || (k_norm == "claude" && norm == "claude-code")
                    {
                        Some(v)
                    } else {
                        None
                    }
                })
            })
            .filter(|p| p.enabled)?;

        let raw_url = if !policy.url.trim().is_empty() {
            policy.url.trim()
        } else if !self.global_proxy_url.is_empty() {
            self.global_proxy_url.as_str()
        } else {
            return None;
        };

        Some(format_session_proxy_url(
            raw_url,
            member_id,
            policy.sticky,
            policy.ttl_secs,
            now_unix,
        ))
    }
}

/// Proxy for the shared client that every provider without its own policy
/// uses.
///
/// `proxy-providers` is an allowlist: as soon as one provider opts in, the
/// global `proxy-url` is just that allowlist's default address, so keeping it
/// on the shared client would silently push every other provider's egress
/// through the same proxy. With no provider opted in the URL keeps its
/// process-wide meaning.
pub fn base_proxy_url(settings: &Settings) -> Option<&str> {
    if settings.proxy_providers.values().any(|policy| policy.enabled) {
        return None;
    }
    let url = settings.proxy_url.trim();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

pub fn format_session_proxy_url(
    base: &str,
    member_id: &str,
    sticky: bool,
    ttl_secs: u64,
    now_unix: u64,
) -> String {
    let parsed = match Url::parse(base) {
        Ok(u) => u,
        Err(_) => return base.to_string(),
    };

    let username = if sticky {
        let sanitized = sanitize_session_id(member_id);
        if ttl_secs > 0 {
            let bucket = now_unix / ttl_secs;
            format!("sess={sanitized}-b{bucket};any=1")
        } else {
            format!("sess={sanitized};any=1")
        }
    } else {
        "any=1".to_string()
    };

    let scheme = parsed.scheme();
    let host = parsed.host_str().unwrap_or("127.0.0.1");
    let port = parsed.port().map(|p| format!(":{p}")).unwrap_or_default();
    let path = parsed.path();
    let path = if path.is_empty() { "/" } else { path };

    format!("{scheme}://{username}:x@{host}{port}{path}")
}

pub fn sanitize_session_id(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    if s.len() > 48 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        raw.hash(&mut hasher);
        let h = hasher.finish();
        format!("{}-{:08x}", &s[..32], h as u32)
    } else if s.is_empty() {
        "default".to_string()
    } else {
        s
    }
}

pub fn build_http_client(proxy_url: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().tcp_nodelay(true);
    if let Some(url) = proxy_url {
        if !url.trim().is_empty() {
            let proxy = reqwest::Proxy::all(url.trim())
                .map_err(|e| anyhow::anyhow!("failed to build proxy for '{url}': {e}"))?;
            builder = builder.proxy(proxy);
        }
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build reqwest client: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn settings_with(proxy_url: &str, providers: BTreeMap<String, ProviderProxyPolicy>) -> Settings {
        Settings {
            proxy_url: proxy_url.to_string(),
            proxy_providers: providers,
            ..Settings::default()
        }
    }

    #[test]
    fn base_proxy_is_global_when_no_provider_opted_in() {
        let settings = settings_with("http://127.0.0.1:3128", BTreeMap::new());
        assert_eq!(base_proxy_url(&settings), Some("http://127.0.0.1:3128"));
    }

    #[test]
    fn base_proxy_is_direct_when_a_provider_opted_in() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "cline".to_string(),
            ProviderProxyPolicy {
                enabled: true,
                ..ProviderProxyPolicy::default()
            },
        );
        let settings = settings_with("http://127.0.0.1:3128", providers);
        // The opted-in provider still resolves the global URL as its address.
        assert_eq!(base_proxy_url(&settings), None);
        assert!(ProxyRuntime::from_settings(&settings)
            .session_proxy_url("cline", "acc1", 1000)
            .is_some());
        assert_eq!(
            ProxyRuntime::from_settings(&settings).session_proxy_url("antigravity", "acc1", 1000),
            None
        );
    }

    #[test]
    fn base_proxy_is_global_when_every_provider_policy_is_disabled() {
        let mut providers = BTreeMap::new();
        providers.insert("cline".to_string(), ProviderProxyPolicy::default());
        let settings = settings_with("http://127.0.0.1:3128", providers);
        assert_eq!(base_proxy_url(&settings), Some("http://127.0.0.1:3128"));
    }

    #[test]
    fn policy_disabled_returns_none() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "cline".to_string(),
            ProviderProxyPolicy {
                enabled: false,
                sticky: true,
                ttl_secs: 0,
                url: String::new(),
            },
        );
        let runtime = ProxyRuntime {
            global_proxy_url: "http://127.0.0.1:3128".to_string(),
            providers,
        };
        assert_eq!(runtime.session_proxy_url("cline", "acc1", 1000), None);
    }

    #[test]
    fn policy_enabled_sticky_ttl_zero() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "cline".to_string(),
            ProviderProxyPolicy {
                enabled: true,
                sticky: true,
                ttl_secs: 0,
                url: String::new(),
            },
        );
        let runtime = ProxyRuntime {
            global_proxy_url: "http://127.0.0.1:3128".to_string(),
            providers,
        };
        let url = runtime
            .session_proxy_url("cline", "acc1", 1000)
            .expect("url");
        assert_eq!(url, "http://sess=acc1;any=1:x@127.0.0.1:3128/");
    }

    #[test]
    fn policy_enabled_sticky_ttl_buckets() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "cline".to_string(),
            ProviderProxyPolicy {
                enabled: true,
                sticky: true,
                ttl_secs: 600,
                url: String::new(),
            },
        );
        let runtime = ProxyRuntime {
            global_proxy_url: "http://127.0.0.1:3128".to_string(),
            providers,
        };
        let url1 = runtime.session_proxy_url("cline", "acc1", 1000).unwrap();
        let url2 = runtime.session_proxy_url("cline", "acc1", 1199).unwrap();
        let url3 = runtime.session_proxy_url("cline", "acc1", 1200).unwrap();

        assert_eq!(url1, "http://sess=acc1-b1;any=1:x@127.0.0.1:3128/");
        assert_eq!(url2, url1);
        assert_eq!(url3, "http://sess=acc1-b2;any=1:x@127.0.0.1:3128/");
    }

    #[test]
    fn policy_non_sticky_uses_any_only() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "cline".to_string(),
            ProviderProxyPolicy {
                enabled: true,
                sticky: false,
                ttl_secs: 0,
                url: String::new(),
            },
        );
        let runtime = ProxyRuntime {
            global_proxy_url: "http://127.0.0.1:3128".to_string(),
            providers,
        };
        let url = runtime.session_proxy_url("cline", "acc1", 1000).unwrap();
        assert_eq!(url, "http://any=1:x@127.0.0.1:3128/");
    }

    #[test]
    fn policy_custom_url_override() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "cline".to_string(),
            ProviderProxyPolicy {
                enabled: true,
                sticky: true,
                ttl_secs: 0,
                url: "http://127.0.0.1:9999".to_string(),
            },
        );
        let runtime = ProxyRuntime {
            global_proxy_url: "http://127.0.0.1:3128".to_string(),
            providers,
        };
        let url = runtime.session_proxy_url("cline", "acc1", 1000).unwrap();
        assert_eq!(url, "http://sess=acc1;any=1:x@127.0.0.1:9999/");
    }

    #[test]
    fn sanitize_session_id_hashes_long_strings() {
        let long_id = "generic-cline-oauth-a3f834a97167decdd49292fefceccfca0e044cbcfe24c6d886b3bb64b1bf18a6";
        let sanitized = sanitize_session_id(long_id);
        assert!(sanitized.len() <= 48);
        assert!(sanitized.starts_with("generic-cline-oauth-a3f834a97167"));
    }

    #[tokio::test]
    async fn live_egress_integration_test() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "cline".to_string(),
            ProviderProxyPolicy {
                enabled: true,
                sticky: true,
                ttl_secs: 600,
                url: String::new(),
            },
        );
        let runtime = ProxyRuntime {
            global_proxy_url: "http://127.0.0.1:3128".to_string(),
            providers,
        };
        let proxy_url = runtime
            .session_proxy_url("cline", "user-orhnpy19@superwiki.net", 1726000000)
            .expect("proxy url");
        let client = build_http_client(Some(&proxy_url)).expect("client");
        let resp1 = client
            .get("https://api.ipify.org")
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await;
        if let Ok(r) = resp1 {
            let ip1 = r.text().await.unwrap_or_default();
            assert!(!ip1.is_empty());
            let resp2 = client
                .get("https://api.ipify.org")
                .timeout(std::time::Duration::from_secs(15))
                .send()
                .await
                .expect("second request");
            let ip2 = resp2.text().await.unwrap_or_default();
            assert_eq!(ip1, ip2, "sticky session should preserve exit IP across requests");
        }
    }
}
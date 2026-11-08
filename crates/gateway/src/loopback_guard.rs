use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

pub const QA_LOOPBACK_ONLY_ENV: &str = "MAHOQUOT_QA_LOOPBACK_ONLY";

pub fn is_qa_loopback_only() -> bool {
    std::env::var(QA_LOOPBACK_ONLY_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

pub fn is_literal_loopback_host(host: &str) -> bool {
    let clean = host.trim().trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = clean.parse::<IpAddr>() {
        ip.is_loopback()
    } else {
        false
    }
}

pub fn is_literal_loopback_url(url: &reqwest::Url) -> bool {
    url.host_str()
        .map(is_literal_loopback_host)
        .unwrap_or(false)
}

#[derive(Debug, Clone, Default)]
pub struct QaLoopbackDnsResolver;

impl QaLoopbackDnsResolver {
    pub fn new() -> Self {
        Self
    }
}

impl reqwest::dns::Resolve for QaLoopbackDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_string();
        if is_literal_loopback_host(&host) {
            let clean = host.trim().trim_start_matches('[').trim_end_matches(']');
            if let Ok(ip) = clean.parse::<IpAddr>() {
                tracing::debug!(
                    target: "mahoquot::loopback_guard",
                    target = %host,
                    decision = "allow",
                    "outbound loopback guard passed literal loopback host"
                );
                let addr = SocketAddr::new(ip, 0);
                return Box::pin(async move {
                    let addrs: reqwest::dns::Addrs = Box::new(std::iter::once(addr));
                    Ok(addrs)
                });
            }
        }

        tracing::warn!(
            target: "mahoquot::loopback_guard",
            target = %host,
            decision = "deny",
            reason = "non-literal-loopback DNS host",
            "outbound loopback guard rejected target before DNS/network"
        );

        Box::pin(async move {
            Err(Box::new(io::Error::other(format!(
                "MAHOQUOT_QA_LOOPBACK_ONLY: outbound DNS/target '{host}' rejected before network (only literal loopback permitted)"
            ))) as Box<dyn std::error::Error + Send + Sync>)
        })
    }
}

pub fn qa_loopback_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        let next_url = attempt.url().clone();
        if !is_literal_loopback_url(&next_url) {
            tracing::warn!(
                target: "mahoquot::loopback_guard",
                target = %next_url,
                decision = "deny",
                reason = "non-literal-loopback redirect target",
                "outbound loopback guard rejected redirect"
            );
            return attempt.error(io::Error::other(format!(
                "MAHOQUOT_QA_LOOPBACK_ONLY: outbound redirect to non-literal-loopback '{next_url}' rejected"
            )));
        }

        if attempt.previous().len() >= 10 {
            tracing::warn!(
                target: "mahoquot::loopback_guard",
                target = %next_url,
                decision = "deny",
                reason = "too many redirects in chain",
                "outbound loopback guard halted redirect loop"
            );
            return attempt.error(io::Error::other(
                "MAHOQUOT_QA_LOOPBACK_ONLY: too many redirects in chain",
            ));
        }

        tracing::debug!(
            target: "mahoquot::loopback_guard",
            target = %next_url,
            decision = "allow",
            "outbound loopback guard allowed redirect"
        );
        attempt.follow()
    })
}

#[derive(Clone, Default)]
pub struct LoopbackGuardLayer;

impl LoopbackGuardLayer {
    pub fn new() -> Self {
        Self
    }
}

impl<S> tower::Layer<S> for LoopbackGuardLayer {
    type Service = LoopbackGuardService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        LoopbackGuardService { inner }
    }
}

#[derive(Clone)]
pub struct LoopbackGuardService<S> {
    inner: S,
}

impl<S, Req> tower::Service<Req> for LoopbackGuardService<S>
where
    S: tower::Service<Req, Error = Box<dyn std::error::Error + Send + Sync>>
        + Clone
        + Send
        + Sync
        + 'static,
    S::Future: Send + 'static,
    Req: std::fmt::Debug + Send + 'static,
{
    type Response = S::Response;
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Req) -> Self::Future {
        let raw = format!("{req:?}");
        let uri_str = raw
            .trim()
            .strip_prefix("Unnameable(")
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or(&raw);

        let is_loopback = if let Ok(uri) = uri_str.parse::<http::Uri>() {
            uri.host().map(is_literal_loopback_host).unwrap_or(false)
        } else {
            false
        };

        if !is_loopback {
            tracing::warn!(
                target: "mahoquot::loopback_guard",
                target = %uri_str,
                decision = "deny",
                reason = "initial non-literal-loopback target rejected before socket connection",
                "outbound loopback guard rejected connection"
            );
            let msg = format!(
                "MAHOQUOT_QA_LOOPBACK_ONLY: outbound connection to non-literal-loopback target '{uri_str}' rejected before socket"
            );
            return Box::pin(async move {
                Err(Box::new(io::Error::other(msg)) as Box<dyn std::error::Error + Send + Sync>)
            });
        }

        tracing::debug!(
            target: "mahoquot::loopback_guard",
            target = %uri_str,
            decision = "allow",
            "outbound loopback guard passed literal loopback connection"
        );

        let fut = self.inner.call(req);
        Box::pin(fut)
    }
}

pub fn apply_qa_loopback_guard(mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    if is_qa_loopback_only() {
        builder = builder
            .dns_resolver(Arc::new(QaLoopbackDnsResolver::new()))
            .redirect(qa_loopback_redirect_policy())
            .connector_layer(LoopbackGuardLayer::new());
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::Redirect;
    use axum::routing::get;
    use axum::Router;
    use std::error::Error as StdError;
    use std::str::FromStr;
    use std::time::Duration;

    #[test]
    fn test_is_literal_loopback_host_accepts_valid_loopback_ips() {
        assert!(is_literal_loopback_host("127.0.0.1"));
        assert!(is_literal_loopback_host("127.0.0.2"));
        assert!(is_literal_loopback_host("127.100.200.1"));
        assert!(is_literal_loopback_host("::1"));
        assert!(is_literal_loopback_host("[::1]"));
        assert!(is_literal_loopback_host(" [::1] "));
    }

    #[test]
    fn test_is_literal_loopback_host_rejects_domains_and_non_loopback_ips() {
        assert!(!is_literal_loopback_host("localhost"));
        assert!(!is_literal_loopback_host("external.invalid"));
        assert!(!is_literal_loopback_host("api.openai.com"));
        assert!(!is_literal_loopback_host("8.8.8.8"));
        assert!(!is_literal_loopback_host("192.168.1.1"));
        assert!(!is_literal_loopback_host("10.0.0.1"));
        assert!(!is_literal_loopback_host("0.0.0.0"));
        assert!(!is_literal_loopback_host(""));
        assert!(!is_literal_loopback_host("   "));
    }

    #[test]
    fn test_is_literal_loopback_url() {
        assert!(is_literal_loopback_url(
            &reqwest::Url::parse("http://127.0.0.1:18841/v1/chat/completions").unwrap()
        ));
        assert!(is_literal_loopback_url(
            &reqwest::Url::parse("http://127.0.0.2:18840/healthz").unwrap()
        ));
        assert!(is_literal_loopback_url(
            &reqwest::Url::parse("http://[::1]:18841/test").unwrap()
        ));

        assert!(!is_literal_loopback_url(
            &reqwest::Url::parse("http://localhost:18841/v1/chat").unwrap()
        ));
        assert!(!is_literal_loopback_url(
            &reqwest::Url::parse("https://external.invalid/backend-api/codex/responses").unwrap()
        ));
        assert!(!is_literal_loopback_url(
            &reqwest::Url::parse("http://8.8.8.8:80/api").unwrap()
        ));
    }

    #[tokio::test]
    async fn test_dns_resolver_rejects_non_literal_hosts() {
        use reqwest::dns::Resolve;

        let resolver = QaLoopbackDnsResolver::new();

        let err = match resolver
            .resolve(reqwest::dns::Name::from_str("external.invalid").unwrap())
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("external.invalid must be rejected"),
        };
        assert!(err.to_string().contains("MAHOQUOT_QA_LOOPBACK_ONLY"));

        let err = match resolver
            .resolve(reqwest::dns::Name::from_str("localhost").unwrap())
            .await
        {
            Err(e) => e,
            Ok(_) => panic!("localhost must be rejected"),
        };
        assert!(err.to_string().contains("MAHOQUOT_QA_LOOPBACK_ONLY"));

        let addrs = resolver
            .resolve(reqwest::dns::Name::from_str("127.0.0.1").unwrap())
            .await
            .expect("127.0.0.1 must resolve");
        let list: Vec<_> = addrs.collect();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].ip(), "127.0.0.1".parse::<IpAddr>().unwrap());
    }

    #[tokio::test]
    async fn test_client_allows_literal_loopback_http_200_positive() {
        let app = Router::new().route("/positive-loopback", get(|| async { "loopback-ok" }));
        let mut listener = None;
        for port in 18840..=18898 {
            if let Ok(l) = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await {
                listener = Some(l);
                break;
            }
        }
        let listener = listener.expect("must bind reserved test port in 18840-18898");
        let addr = listener.local_addr().unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let mut server_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        let client = reqwest::Client::builder()
            .dns_resolver(Arc::new(QaLoopbackDnsResolver::new()))
            .redirect(qa_loopback_redirect_policy())
            .connector_layer(LoopbackGuardLayer::new())
            .build()
            .unwrap();

        let send_fut = client
            .get(format!(
                "http://127.0.0.1:{}/positive-loopback",
                addr.port()
            ))
            .send();
        let res = tokio::time::timeout(Duration::from_secs(5), send_fut)
            .await
            .expect("request send timed out")
            .expect("literal loopback request must succeed");

        assert_eq!(res.status(), reqwest::StatusCode::OK);
        let body = tokio::time::timeout(Duration::from_secs(5), res.text())
            .await
            .expect("body read timed out")
            .expect("body read must succeed");
        assert_eq!(body, "loopback-ok");

        let _ = shutdown_tx.send(());
        match tokio::time::timeout(Duration::from_secs(5), &mut server_task).await {
            Ok(join_res) => {
                let server_res = join_res.expect("server task panicked");
                server_res.expect("server run failed");
            }
            Err(_) => {
                server_task.abort();
                let _ = server_task.await;
                panic!("server task shutdown timed out");
            }
        }
    }

    #[tokio::test]
    async fn test_client_rejects_redirect_to_external_invalid() {
        let (redirect_signal_tx, redirect_signal_rx) = tokio::sync::oneshot::channel::<()>();
        let redirect_signal_tx =
            std::sync::Arc::new(tokio::sync::Mutex::new(Some(redirect_signal_tx)));

        let app = Router::new().route(
            "/redirect-external",
            get({
                let redirect_signal_tx = redirect_signal_tx.clone();
                move || {
                    let redirect_signal_tx = redirect_signal_tx.clone();
                    async move {
                        if let Some(tx) = redirect_signal_tx.lock().await.take() {
                            let _ = tx.send(());
                        }
                        Redirect::temporary("https://external.invalid/backend-api/codex/responses")
                    }
                }
            }),
        );
        let mut listener = None;
        for port in 18840..=18898 {
            if let Ok(l) = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await {
                listener = Some(l);
                break;
            }
        }
        let listener = listener.expect("must bind reserved test port in 18840-18898");
        let addr = listener.local_addr().unwrap();

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let mut server_task = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
        });

        let client = reqwest::Client::builder()
            .dns_resolver(Arc::new(QaLoopbackDnsResolver::new()))
            .redirect(qa_loopback_redirect_policy())
            .connector_layer(LoopbackGuardLayer::new())
            .build()
            .unwrap();

        let send_fut = client
            .get(format!(
                "http://127.0.0.1:{}/redirect-external",
                addr.port()
            ))
            .send();
        let res = tokio::time::timeout(Duration::from_secs(5), send_fut)
            .await
            .expect("request send timed out");

        let redirect_event_occurred =
            tokio::time::timeout(Duration::from_secs(2), redirect_signal_rx)
                .await
                .expect("redirect handler did not fire in time")
                .is_ok();
        assert!(
            redirect_event_occurred,
            "302 redirect event must be signaled before redirect evaluation"
        );

        let err = match res {
            Err(e) => e,
            Ok(_) => panic!("redirect to external.invalid must be rejected"),
        };

        let mut found_redirect_cause = false;
        let mut curr: Option<&(dyn StdError + 'static)> = Some(&err);
        while let Some(e) = curr {
            let msg = e.to_string();
            if msg.contains("outbound redirect to non-literal-loopback") {
                found_redirect_cause = true;
                break;
            }
            curr = e.source();
        }
        assert!(
            found_redirect_cause,
            "Error source chain missing redirect-specific cause: {err:?}"
        );

        let _ = shutdown_tx.send(());
        match tokio::time::timeout(Duration::from_secs(5), &mut server_task).await {
            Ok(join_res) => {
                let server_res = join_res.expect("server task panicked");
                server_res.expect("server run failed");
            }
            Err(_) => {
                server_task.abort();
                let _ = server_task.await;
                panic!("server task shutdown timed out");
            }
        }
    }

    #[tokio::test]
    async fn test_client_rejects_initial_non_loopback_ip_with_zero_port_18899_traffic() {
        let dummy_listener = tokio::net::TcpListener::bind("127.0.0.1:18899")
            .await
            .expect("bind 18899 must succeed");

        let client = reqwest::Client::builder()
            .dns_resolver(Arc::new(QaLoopbackDnsResolver::new()))
            .redirect(qa_loopback_redirect_policy())
            .connector_layer(LoopbackGuardLayer::new())
            .build()
            .unwrap();

        let send_fut = client.get("http://8.8.8.8:80/unauthorized-external").send();
        let res = tokio::time::timeout(Duration::from_secs(5), send_fut)
            .await
            .expect("request send timed out");

        let err = match res {
            Err(e) => e,
            Ok(_) => panic!("initial connection to non-loopback IP 8.8.8.8 must be rejected"),
        };

        let mut found_connection_cause = false;
        let mut curr: Option<&(dyn StdError + 'static)> = Some(&err);
        while let Some(e) = curr {
            let msg = e.to_string();
            if msg.contains("outbound connection to non-literal-loopback target") {
                found_connection_cause = true;
                break;
            }
            curr = e.source();
        }
        assert!(
            found_connection_cause,
            "Error source chain missing connection-specific cause: {err:?}"
        );

        let std_listener = dummy_listener.into_std().expect("into_std must succeed");
        std_listener
            .set_nonblocking(true)
            .expect("set_nonblocking must succeed");
        match std_listener.accept() {
            Ok((_stream, addr)) => {
                panic!("unexpected connection arrived at port 18899 from {addr}");
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => {
                panic!("unexpected error checking port 18899: {e}");
            }
        }
    }
}

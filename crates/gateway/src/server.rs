use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::extract::ConnectInfo;
use axum::{Extension, Router};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio_rustls::rustls;
use tokio_rustls::TlsAcceptor;

use crate::management::gate::ManagementAttempts;
use crate::management::settings::Settings;
use crate::management::store::SettingsStore;

pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub trait ConnectionIo: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + Unpin> ConnectionIo for T {}

pub type ProtocolStream = BufReader<Box<dyn ConnectionIo>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Protocol {
    Http,
    Resp,
}

pub fn protocol_prefix(byte: u8) -> Protocol {
    match byte {
        b'*' | b'$' | b'+' | b'-' | b':' => Protocol::Resp,
        _ => Protocol::Http,
    }
}

pub async fn sniff(stream: &mut ProtocolStream) -> anyhow::Result<Protocol> {
    let prefix = stream.fill_buf().await?;
    let byte = prefix
        .first()
        .context("connection closed before protocol sniff")?;
    Ok(protocol_prefix(*byte))
}

#[derive(Clone, Debug, Default)]
pub struct TlsSettings {
    pub enable: bool,
    pub cert: String,
    pub key: String,
}

impl TlsSettings {
    pub fn from_settings(settings: &Settings) -> anyhow::Result<Self> {
        let enable = settings
            .tls
            .extra
            .get("enable")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if let Some(val) = settings.tls.extra.get("enable") {
            if !val.is_boolean() {
                anyhow::bail!("tls.enable must be a boolean");
            }
        }
        let cert = settings
            .tls
            .extra
            .get("cert")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let key = settings
            .tls
            .extra
            .get("key")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();

        Ok(Self { enable, cert, key })
    }

    pub async fn prepare(&self) -> anyhow::Result<Option<TlsAcceptor>> {
        if !self.enable {
            return Ok(None);
        }
        anyhow::ensure!(
            !self.cert.trim().is_empty(),
            "tls.cert is required when TLS is enabled"
        );
        anyhow::ensure!(
            !self.key.trim().is_empty(),
            "tls.key is required when TLS is enabled"
        );
        let cert = tokio::fs::read(&self.cert)
            .await
            .context("cannot read tls.cert")?;
        let key = tokio::fs::read(&self.key)
            .await
            .context("cannot read tls.key")?;
        Ok(Some(TlsAcceptor::from(Arc::new(tls_config(&cert, &key)?))))
    }
}

fn tls_config(cert: &[u8], key: &[u8]) -> anyhow::Result<rustls::ServerConfig> {
    let certs = rustls_pemfile::certs(&mut std::io::Cursor::new(cert))
        .collect::<Result<Vec<_>, _>>()
        .context("invalid TLS certificate PEM")?;
    let key = rustls_pemfile::private_key(&mut std::io::Cursor::new(key))
        .context("invalid TLS private key PEM")?
        .context("TLS private key PEM is empty")?;
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .context("invalid TLS certificate/key pair")?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

pub struct ProtocolListener {
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    settings: Option<Arc<SettingsStore>>,
    attempts: Arc<std::sync::Mutex<ManagementAttempts>>,
    hub: Arc<crate::resp::RedisQueueHub>,
    updates: watch::Receiver<Settings>,
    updates_open: bool,
    connections: JoinSet<anyhow::Result<(ProtocolStream, SocketAddr)>>,
}

impl ProtocolListener {
    pub async fn new(listener: TcpListener, settings: &SettingsStore) -> anyhow::Result<Self> {
        let (tx, updates) = watch::channel((*settings.current()).clone());
        settings.add_observer(Arc::new(move |next: &Settings| {
            let _ = tx.send(next.clone());
        }));
        let initial = (*settings.current()).clone();
        let tls = TlsSettings::from_settings(&initial)?.prepare().await?;
        Ok(Self {
            listener,
            tls,
            settings: None,
            attempts: Arc::new(std::sync::Mutex::new(ManagementAttempts::default())),
            hub: crate::resp::global_redis_hub().clone(),
            updates,
            updates_open: true,
            connections: JoinSet::new(),
        })
    }

    pub fn with_management_context(
        mut self,
        settings: Arc<SettingsStore>,
        attempts: Arc<std::sync::Mutex<ManagementAttempts>>,
    ) -> Self {
        self.settings = Some(settings);
        self.attempts = attempts;
        self
    }
}

pub async fn reload_tls(
    active: &mut Option<TlsAcceptor>,
    settings: &Settings,
) -> anyhow::Result<()> {
    let next = TlsSettings::from_settings(settings)?;
    let prepared = next.prepare().await?;
    *active = prepared;
    Ok(())
}

impl axum::serve::Listener for ProtocolListener {
    type Io = ProtocolStream;
    type Addr = SocketAddr;

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.listener.local_addr()
    }

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            tokio::select! {
                biased;

                change = self.updates.changed(), if self.updates_open => {
                    match change {
                        Ok(()) => {
                            let next = self.updates.borrow().clone();
                            if let Err(e) = reload_tls(&mut self.tls, &next).await {
                                tracing::warn!(error = %e, "TLS configuration reload rejected; retaining previous listener configuration");
                            }
                        }
                        Err(_) => {
                            self.updates_open = false;
                        }
                    }
                }

                Some(result) = self.connections.join_next(), if !self.connections.is_empty() => {
                    match result {
                        Ok(Ok((stream, peer))) => return (stream, peer),
                        Ok(Err(_)) => {}
                        Err(_) => {}
                    }
                }

                accepted = self.listener.accept() => {
                    match accepted {
                        Ok((stream, peer)) => {
                            let tls = self.tls.clone();
                            let settings = self.settings.clone();
                            let attempts = self.attempts.clone();
                            let hub = self.hub.clone();
                            self.connections.spawn(async move {
                                tokio::time::timeout(HANDSHAKE_TIMEOUT, async move {
                                    let io: Box<dyn ConnectionIo> = match tls {
                                        Some(acceptor) => Box::new(acceptor.accept(stream).await?),
                                        None => Box::new(stream),
                                    };
                                    let mut stream = BufReader::new(io);
                                    match sniff(&mut stream).await? {
                                        Protocol::Http => Ok((stream, peer)),
                                        Protocol::Resp => {
                                            tokio::spawn(async move {
                                                crate::resp::handle_redis_connection(stream, peer, settings, attempts, hub).await;
                                            });
                                            anyhow::bail!("handled as resp");
                                        }
                                    }
                                }).await.context("protocol handshake timed out")?
                            });
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "TCP accept failed");
                        }
                    }
                }
            }
        }
    }
}

pub async fn run_protocol_server(
    listener: ProtocolListener,
    app: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let make_service = tower::service_fn(
        move |stream: axum::serve::IncomingStream<'_, ProtocolListener>| {
            let app = app
                .clone()
                .layer(Extension(ConnectInfo(*stream.remote_addr())));
            std::future::ready(Ok::<_, std::convert::Infallible>(app))
        },
    );
    axum::serve(listener, make_service)
        .with_graceful_shutdown(shutdown)
        .await
        .map_err(|e| anyhow::anyhow!("server error: {}", e))
}

pub async fn run_server(
    listener: TcpListener,
    app: Router,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
    .map_err(|e| anyhow::anyhow!("server error: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::path::PathBuf;
    use tokio::io::AsyncReadExt;

    fn fixture_path(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join(name)
    }

    #[test]
    fn test_disabled_defaults() {
        let settings = Settings::default();
        let tls = TlsSettings::from_settings(&settings).expect("default settings must parse");
        assert!(!tls.enable);
        assert!(tls.cert.is_empty());
        assert!(tls.key.is_empty());
    }

    #[test]
    fn test_invalid_types() {
        let mut settings = Settings::default();
        settings
            .tls
            .extra
            .insert("enable".to_string(), serde_json::json!("not-a-bool"));
        let err = TlsSettings::from_settings(&settings).unwrap_err();
        assert!(err.to_string().contains("tls.enable must be a boolean"));
    }

    #[tokio::test]
    async fn test_required_cert_fields() {
        let mut settings = Settings::default();
        settings
            .tls
            .extra
            .insert("enable".to_string(), serde_json::json!(true));
        let tls = TlsSettings::from_settings(&settings).unwrap();
        let err = match tls.prepare().await {
            Ok(_) => panic!("should fail"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("tls.cert is required"));

        let mut settings2 = Settings::default();
        settings2
            .tls
            .extra
            .insert("enable".to_string(), serde_json::json!(true));
        settings2
            .tls
            .extra
            .insert("cert".to_string(), serde_json::json!("some-cert.pem"));
        let tls2 = TlsSettings::from_settings(&settings2).unwrap();
        let err2 = match tls2.prepare().await {
            Ok(_) => panic!("should fail"),
            Err(e) => e,
        };
        assert!(err2.to_string().contains("tls.key is required"));
    }

    #[test]
    fn test_malformed_pem() {
        let bad_cert = b"not-a-valid-cert-pem";
        let bad_key = b"not-a-valid-key-pem";
        let err = tls_config(bad_cert, bad_key).unwrap_err();
        assert!(err.to_string().contains("TLS private key") || err.to_string().contains("PEM"));
    }

    #[test]
    fn test_mismatched_cert_key() {
        let cert_pem = std::fs::read(fixture_path("listener-cert.pem")).unwrap();
        let rsa_key = std::fs::read(fixture_path("test-rsa-private.pem")).unwrap();
        let err = tls_config(&cert_pem, &rsa_key).unwrap_err();
        assert!(
            err.to_string().contains("invalid TLS certificate/key pair")
                || err.to_string().contains("pair")
        );
    }

    #[test]
    fn test_valid_alpn() {
        let cert_pem = std::fs::read(fixture_path("listener-cert.pem")).unwrap();
        let key_pem = std::fs::read(fixture_path("listener-key.pem")).unwrap();
        let config = tls_config(&cert_pem, &key_pem).expect("valid cert and key");
        assert_eq!(
            config.alpn_protocols,
            vec![b"h2".to_vec(), b"http/1.1".to_vec()]
        );
    }

    #[tokio::test]
    async fn test_failed_reload_preservation() {
        let cert_path = fixture_path("listener-cert.pem")
            .to_string_lossy()
            .to_string();
        let key_path = fixture_path("listener-key.pem")
            .to_string_lossy()
            .to_string();

        let mut settings = Settings::default();
        settings
            .tls
            .extra
            .insert("enable".to_string(), serde_json::json!(true));
        settings
            .tls
            .extra
            .insert("cert".to_string(), serde_json::json!(cert_path));
        settings
            .tls
            .extra
            .insert("key".to_string(), serde_json::json!(key_path));

        let initial_tls = TlsSettings::from_settings(&settings).unwrap();
        let mut active = initial_tls.prepare().await.unwrap();
        assert!(active.is_some());

        // Invalid candidate
        let mut invalid_settings = Settings::default();
        invalid_settings
            .tls
            .extra
            .insert("enable".to_string(), serde_json::json!(true));
        invalid_settings.tls.extra.insert(
            "cert".to_string(),
            serde_json::json!("/nonexistent/cert.pem"),
        );
        invalid_settings
            .tls
            .extra
            .insert("key".to_string(), serde_json::json!("/nonexistent/key.pem"));

        let res = reload_tls(&mut active, &invalid_settings).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn test_disabling_tls() {
        let mut active = Some(TlsAcceptor::from(Arc::new(
            tls_config(
                &std::fs::read(fixture_path("listener-cert.pem")).unwrap(),
                &std::fs::read(fixture_path("listener-key.pem")).unwrap(),
            )
            .unwrap(),
        )));
        assert!(active.is_some());

        let mut settings = Settings::default();
        settings
            .tls
            .extra
            .insert("enable".to_string(), serde_json::json!(false));
        reload_tls(&mut active, &settings).await.unwrap();
        assert!(active.is_none());
    }

    #[test]
    fn test_http2_preface_classification() {
        let preface = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
        assert_eq!(protocol_prefix(preface[0]), Protocol::Http);
        assert_eq!(preface[0], b'P');
    }

    #[test]
    fn test_canonical_resp_prefixes() {
        for prefix in *b"*$+-:" {
            assert_eq!(protocol_prefix(prefix), Protocol::Resp);
        }
        for prefix in *b"GPHDOC" {
            assert_eq!(protocol_prefix(prefix), Protocol::Http);
        }
    }

    #[tokio::test]
    async fn test_lossless_sniffing() {
        let payload = b"GET /v1/models HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let cursor: Box<dyn ConnectionIo> = Box::new(Cursor::new(payload.to_vec()));
        let mut stream = BufReader::new(cursor);

        let proto = sniff(&mut stream).await.unwrap();
        assert_eq!(proto, Protocol::Http);

        let mut read_back = Vec::new();
        stream.read_to_end(&mut read_back).await.unwrap();
        assert_eq!(read_back, payload);
    }

    #[tokio::test]
    async fn test_idle_sniff_expiration_with_paused_clock() {
        tokio::time::pause();

        let (client, server) = tokio::io::duplex(64);
        let io: Box<dyn ConnectionIo> = Box::new(server);
        let mut stream = BufReader::new(io);

        let sniff_fut = sniff(&mut stream);
        tokio::pin!(sniff_fut);

        // An idle peer never sends a byte, so the sniff must be cut off by the
        // handshake deadline rather than complete.
        let outcome = tokio::time::timeout(HANDSHAKE_TIMEOUT, &mut sniff_fut).await;
        assert!(
            outcome.is_err(),
            "idle sniff must expire at the handshake deadline"
        );

        drop(client);
    }
}

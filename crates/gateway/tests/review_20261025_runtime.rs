use axum::{extract::Request, http::StatusCode, response::IntoResponse, Extension, Router};
use mahoquot_gateway::{
    management::{gate::ManagementAttempts, settings::Settings, store::SettingsStore},
    server::{run_protocol_server, ProtocolListener},
};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

async fn listener() -> tokio::net::TcpListener {
    for port in 18840..=18899 {
        if let Ok(listener) =
            tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await
        {
            return listener;
        }
    }
    panic!("no reserved fixture port available");
}

#[tokio::test]
async fn resp_rejection_keeps_http_healthy_and_shutdown_closes_sessions() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut settings = Settings::default();
        settings.remote_management.secret_key = "runtime-fixture".into();
        let store = Arc::new(SettingsStore::new(settings, Default::default()));
        let tcp = listener().await;
        let addr = tcp.local_addr().unwrap();
        let protocol = ProtocolListener::new(tcp, &store)
            .await
            .unwrap()
            .with_management_context(store, Arc::new(Mutex::new(ManagementAttempts::default())));
        let app = Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(run_protocol_server(protocol, app, async move {
            rx.await.unwrap();
        }));
        let mut attacker =
            tokio::io::BufReader::new(tokio::net::TcpStream::connect(addr).await.unwrap());
        attacker
            .write_all(b"*9223372036854775807\r\n")
            .await
            .unwrap();
        let mut line = String::new();
        attacker.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("-ERR"));
        assert_eq!(
            attacker.read_u8().await.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        assert_eq!(
            client
                .get(format!("http://{addr}/healthz"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap(),
            "ok"
        );
        let mut clients = Vec::new();
        for _ in 0..100 {
            let mut stream =
                tokio::io::BufReader::new(tokio::net::TcpStream::connect(addr).await.unwrap());
            stream
                .write_all(b"*2\r\n$4\r\nAUTH\r\n$15\r\nruntime-fixture\r\n")
                .await
                .unwrap();
            line.clear();
            stream.read_line(&mut line).await.unwrap();
            assert_eq!(line, "+OK\r\n");
            clients.push(stream);
        }
        let subscriber = &mut clients[0];
        subscriber
            .write_all(b"*2\r\n$9\r\nSUBSCRIBE\r\n$6\r\nerrors\r\n")
            .await
            .unwrap();
        assert_eq!(
            mahoquot_gateway::resp::read_resp_array(subscriber)
                .await
                .unwrap(),
            ["subscribe", "errors", "1"]
        );
        tx.send(()).unwrap();
        tasks.join_next().await.unwrap().unwrap().unwrap();
        for mut stream in clients {
            assert_eq!(
                stream.read_u8().await.unwrap_err().kind(),
                std::io::ErrorKind::UnexpectedEof
            );
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn home_tls_honors_override_and_rejects_wrong_name() {
    use mahoquot_gateway::home::{HomeClient, HomeConfig, HomeTLSConfig};
    use tokio_rustls::{rustls, TlsAcceptor};
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let cert = std::fs::read(fixture.join("listener-cert.pem")).unwrap();
    let key = std::fs::read(fixture.join("listener-key.pem")).unwrap();
    let certs = rustls_pemfile::certs(&mut &cert[..])
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = rustls_pemfile::private_key(&mut &key[..]).unwrap().unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(config));
    tokio::time::timeout(Duration::from_secs(10), async {
        for (name, success) in [("localhost", true), ("wrong.fixture.invalid", false)] {
            let tcp = listener().await;
            let port = tcp.local_addr().unwrap().port();
            let acceptor = acceptor.clone();
            let mut tasks = tokio::task::JoinSet::new();
            tasks.spawn(async move {
                let (socket, _) = tcp.accept().await.unwrap();
                match acceptor.accept(socket).await {
                    Ok(stream) => {
                        assert!(success);
                        assert_eq!(stream.get_ref().1.server_name(), Some("localhost"));
                        let mut stream = tokio::io::BufReader::new(stream);
                        assert_eq!(
                            mahoquot_gateway::resp::read_resp_array(&mut stream)
                                .await
                                .unwrap(),
                            ["GET", "config"]
                        );
                        stream.write_all(b"$2\r\n{}\r\n").await.unwrap();
                        stream.flush().await.unwrap();
                    }
                    Err(_) => assert!(!success),
                }
            });
            let client = HomeClient::new(HomeConfig {
                enabled: true,
                host: "127.0.0.1".into(),
                port,
                tls: HomeTLSConfig {
                    enable: true,
                    server_name: name.into(),
                    use_target_server_name: false,
                    ca_cert: fixture
                        .join("listener-cert.pem")
                        .to_string_lossy()
                        .into_owned(),
                    ..Default::default()
                },
                ..Default::default()
            });
            let result = tokio::task::spawn_blocking(move || client.get_config())
                .await
                .unwrap();
            if success {
                assert_eq!(result.unwrap(), b"{}");
            } else {
                assert!(result.is_err());
            }
            tasks.join_next().await.unwrap().unwrap();
        }
    })
    .await
    .unwrap();
}

struct TempRoot(std::path::PathBuf);
impl Drop for TempRoot {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.0).expect("remove owned fixture root");
    }
}

#[tokio::test]
async fn video_reads_use_creator_credentials_and_revalidate_policy() {
    use mahoquot_gateway::{
        config::GatewayConfig,
        inbound::{AuthIdentity, ResolvedAuth},
        media,
        state::AppState,
    };
    use mahoquot_registry::{
        CatalogSource, ModelAliasRule, ModelCapability, ModelDescriptor, ModelId, ProviderBinding,
        ProviderId, ProviderPolicy,
    };
    use serde_json::json;
    tokio::time::timeout(Duration::from_secs(15), async {
        let tcp = listener().await;
        let upstream = format!("http://{}", tcp.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let captured = seen.clone();
        let mock = Router::new().fallback(move |request: Request| {
            let seen = captured.clone();
            async move {
                seen.lock().unwrap().push(request.headers()["authorization"].to_str().unwrap().to_owned());
                if request.uri().path().ends_with("/content") {
                    return ([("content-type", "video/mp4")], "fixture-video").into_response();
                }
                axum::Json(json!({"id":"runtime-video", "status":"completed"})).into_response()
            }
        });
        let mut tasks = tokio::task::JoinSet::new();
        tasks.spawn(async move { axum::serve(tcp, mock).await.unwrap(); });
        let root = TempRoot(std::env::temp_dir().join(format!("review-runtime-{}", uuid::Uuid::new_v4())));
        std::fs::create_dir_all(&root.0).unwrap();
        for key in ["account-a", "account-b"] {
            std::fs::write(root.0.join(format!("generic-{key}.json")), json!({"type":"generic", "provider":"runtime-video-provider", "adapter":"openai-chat", "api_key":key, "base_url":upstream, "upstream_override":upstream, "models":["runtime-video-model"]}).to_string()).unwrap();
        }
        let state = Arc::new(AppState::new(&GatewayConfig { auth_dir:root.0.clone(), config_path:root.0.join("config.yaml"), auth_refresh_enabled:false, ..Default::default() }).unwrap());
        let mut registry = mahoquot_registry::embedded_registry_snapshot().unwrap();
        let provider = ProviderId::new("runtime-video-provider").unwrap();
        registry.providers.insert(provider.clone(), ProviderPolicy::Closed);
        let binding = ProviderBinding::new(provider.clone(), ProviderPolicy::Closed, CatalogSource::RemoteSigned).with_capabilities([ModelCapability::Video]);
        let mut model = ModelDescriptor::new(ModelId::new("runtime-video-model").unwrap(), "fixture");
        model.bindings.insert(provider.clone(), binding);
        registry.models.insert(model.id.clone(), model);
        registry.aliases.insert(ModelId::new("runtime-video-alias").unwrap(), ModelAliasRule { alias:ModelId::new("runtime-video-alias").unwrap(), target:ModelId::new("runtime-video-model").unwrap(), provider_id:Some(provider) });
        state.runtime_state().update_registry(Arc::new(registry)).unwrap();
        let gateway = listener().await;
        let base = format!("http://{}", gateway.local_addr().unwrap());
        let auth = ResolvedAuth { identity:AuthIdentity::Master, key_identifier:Some("owner".into()) };
        let routes = Router::new()
            .route("/v1/videos", axum::routing::post(media::videos))
            .route("/v1/videos/{id}", axum::routing::get(media::videos_by_id))
            .route("/openai/videos/{id}", axum::routing::get(media::openai_videos))
            .route("/openai/videos/{id}/content", axum::routing::get(media::openai_videos))
            .layer(Extension(auth.clone())).with_state(state.clone());
        tasks.spawn(async move { axum::serve(gateway, routes).await.unwrap(); });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let created = client.post(format!("{base}/v1/videos")).json(&json!({"model":"runtime-video-alias", "prompt":"fixture"})).send().await.unwrap();
        assert_eq!(created.status(), StatusCode::OK, "{}", created.text().await.unwrap());
        let owner = seen.lock().unwrap()[0].clone();
        for _ in 0..3 {
            for path in ["v1/videos/runtime-video", "openai/videos/runtime-video", "openai/videos/runtime-video/content"] {
                let response = client.get(format!("{base}/{path}")).send().await.unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let body = response.bytes().await.unwrap();
                if path.ends_with("/content") { assert_eq!(body.as_ref(), b"fixture-video"); }
            }
        }
        assert_eq!(seen.lock().unwrap().len(), 10);
        assert!(seen.lock().unwrap().iter().all(|value| value == &owner));
        let denied = media::videos_by_id(axum::extract::State(state.clone()), Extension(ResolvedAuth { identity:AuthIdentity::Master, key_identifier:Some("other".into()) }), axum::extract::Path("runtime-video".into())).await;
        assert_eq!(denied.status(), StatusCode::NOT_FOUND);
        let members = state.pool.load_full();
        for member in &members.members { member.unsupported_models.write().unwrap().push("runtime-video-model".into()); }
        let revoked = client.get(format!("{base}/v1/videos/runtime-video")).send().await.unwrap();
        assert!(!revoked.status().is_success());
        assert_eq!(seen.lock().unwrap().len(), 10);
        tasks.shutdown().await;
        drop(members);
        drop(state);
    }).await.unwrap();
}

//! Test-only inventory oracle. Calls the real handlers: no copied redaction,
//! extra endpoint or authentication bypass. Network protocol probes use only
//! owned loopback listeners in the QA port range.
use super::settings::{ModelCatalogSettings, OmoProfile, Settings};
use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(std::time::Duration::from_secs(15), future)
        .await
        .expect("inventory event deadline")
}

struct ListenerTask(tokio::task::JoinHandle<()>);
impl Drop for ListenerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl ListenerTask {
    async fn close(mut self) {
        self.0.abort();
        assert!(bounded(&mut self.0)
            .await
            .expect_err("aborted listener")
            .is_cancelled());
    }
}

async fn listener() -> tokio::net::TcpListener {
    for port in 18840..=18899 {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => return listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(error) => panic!("inventory listener: {error}"),
        }
    }
    panic!("inventory QA ports exhausted")
}

async fn media_peer() -> Arc<webrtc::peer_connection::RTCPeerConnection> {
    let mut engine = webrtc::api::media_engine::MediaEngine::default();
    engine.register_default_codecs().expect("media codecs");
    let mut settings = webrtc::api::setting_engine::SettingEngine::default();
    settings.set_udp_network(webrtc::ice::udp_network::UDPNetwork::Ephemeral(
        webrtc::ice::udp_network::EphemeralUDP::new(18840, 18899).expect("QA UDP range"),
    ));
    settings.set_include_loopback_candidate(true);
    settings.set_ip_filter(Box::new(|ip| ip.is_loopback()));
    Arc::new(
        webrtc::api::APIBuilder::new()
            .with_media_engine(engine)
            .with_setting_engine(settings)
            .build()
            .new_peer_connection(Default::default())
            .await
            .expect("media peer"),
    )
}

async fn local_sdp(peer: &webrtc::peer_connection::RTCPeerConnection, offer: bool) -> String {
    let mut gathered = peer.gathering_complete_promise().await;
    let description = if offer {
        peer.create_offer(None).await
    } else {
        peer.create_answer(None).await
    }
    .expect("local SDP");
    peer.set_local_description(description)
        .await
        .expect("publish local SDP");
    bounded(gathered.recv()).await;
    peer.local_description()
        .await
        .expect("gathered local SDP")
        .sdp
}

async fn websocket_capture(url: String, key: &str) -> Value {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
    let mut request = url.clone().into_client_request().expect("WS request");
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {key}").parse().unwrap());
    let (mut client, response) = bounded(tokio_tungstenite::connect_async(request))
        .await
        .expect("downstream upgrade");
    assert_eq!(response.status().as_u16(), 101);
    let sent =
        json!({"type":"inventory.echo", "nonce":uuid::Uuid::new_v4().to_string()}).to_string();
    bounded(client.send(Message::Text(sent.clone().into())))
        .await
        .expect("WS send");
    let received = bounded(client.next())
        .await
        .expect("WS echo frame")
        .expect("WS receive");
    assert_eq!(received.to_text().expect("WS text"), sent);
    bounded(client.close(None)).await.expect("WS close");
    json!({"url":url, "downstream_status":101, "sent_utf8":sent, "received_utf8":received.to_text().unwrap()})
}

async fn protocol_inventory_fixture() -> Value {
    assert!(
        crate::loopback_guard::is_qa_loopback_only(),
        "native protocol fixture requires loopback-only runner"
    );
    use axum::{
        extract::{State, WebSocketUpgrade},
        response::IntoResponse,
        routing::{get, post},
        Json, Router,
    };
    use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
    #[derive(Clone)]
    struct Upstream {
        events: tokio::sync::mpsc::UnboundedSender<Value>,
        peers: Arc<tokio::sync::Mutex<Vec<Arc<webrtc::peer_connection::RTCPeerConnection>>>>,
    }
    async fn upgrade(
        State(upstream): State<Upstream>,
        uri: axum::http::Uri,
        ws: WebSocketUpgrade,
    ) -> impl IntoResponse {
        ws.on_upgrade(move |mut socket| async move {
            upstream
                .events
                .send(json!({"kind":"upgrade", "path":uri.path(), "status":101}))
                .expect("upstream capture");
            while let Some(message) = socket.recv().await {
                let message = message.expect("upstream WS frame");
                if matches!(message, axum::extract::ws::Message::Close(_)) {
                    break;
                }
                if socket.send(message).await.is_err() {
                    break;
                }
            }
        })
    }
    async fn offer(State(upstream): State<Upstream>, Json(body): Json<Value>) -> impl IntoResponse {
        let peer = media_peer().await;
        let sdp = body["sdp"].as_str().expect("real upstream offer");
        peer.set_remote_description(
            RTCSessionDescription::offer(sdp.to_owned()).expect("offer SDP"),
        )
        .await
        .expect("accept relay offer");
        let answer = local_sdp(&peer, false).await;
        let location = format!("/v1/realtime/calls/inventory-{}", uuid::Uuid::new_v4());
        upstream.peers.lock().await.push(peer);
        upstream
            .events
            .send(
                json!({"kind":"offer", "path":"/v1/realtime/calls", "status":201,
            "location":location, "offer_sdp":sdp, "answer_sdp":answer}),
            )
            .expect("offer capture");
        (
            axum::http::StatusCode::CREATED,
            [
                ("location", location),
                ("content-type", "application/sdp".into()),
            ],
            answer,
        )
    }
    let upstream_listener = listener().await;
    let upstream_url = format!("http://{}", upstream_listener.local_addr().unwrap());
    let (events, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let upstream = Upstream {
        events,
        peers: Default::default(),
    };
    let upstream_app = Router::new()
        .route("/backend-api/codex/responses", get(upgrade))
        .route("/v1/realtime", get(upgrade))
        .route("/v1/realtime/calls", post(offer))
        .route("/v1/realtime/calls/{id}", get(upgrade))
        .route(
            "/v1/realtime/calls/{id}/hangup",
            post(|| async { axum::http::StatusCode::OK }),
        )
        .with_state(upstream.clone());
    let upstream_task = ListenerTask(tokio::spawn(async move {
        axum::serve(upstream_listener, upstream_app)
            .await
            .expect("upstream serve");
    }));
    let path = std::env::temp_dir().join(format!("inventory-protocol-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&path).expect("protocol directory");
    let dir = FixtureDirectory(path);
    let auth = dir.0.join("auth");
    std::fs::create_dir(&auth).expect("protocol auth directory");
    std::fs::write(
        auth.join("codex-inventory.json"),
        json!({
            "type":"codex", "identity_slug":"inventory", "account_id":"inventory",
            "access_token":"inventory-token", "refresh_token":"fixture", "id_token":"fixture",
            "email":"inventory@example.invalid", "expired":"2099-01-01T00:00:00Z",
            "last_refresh":"", "upstream_override":upstream_url
        })
        .to_string(),
    )
    .expect("protocol credential");
    let config_path = dir.0.join("config.yaml");
    std::fs::write(
        &config_path,
        "api-keys: [inventory-owner, inventory-other]\nmodel-catalog: {refresh-enabled: false}\n",
    )
    .expect("protocol config");
    let state = Arc::new(
        crate::state::AppState::new(&crate::config::GatewayConfig {
            auth_dir: auth,
            config_path,
            auth_refresh_enabled: false,
            catalog_cache_path: Some(dir.0.join("catalog-cache")),
            ..Default::default()
        })
        .expect("protocol state"),
    );
    let gateway_listener = listener().await;
    let address = gateway_listener.local_addr().unwrap();
    let app = crate::routes::create_app(state);
    let gateway_task = ListenerTask(tokio::spawn(async move {
        axum::serve(
            gateway_listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("gateway serve");
    }));
    let mut captures = Vec::new();
    let worker_capture = {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
        let mut request = format!("ws://{address}/v1/ws")
            .into_client_request()
            .expect("worker request");
        request
            .headers_mut()
            .insert("authorization", "Bearer inventory-owner".parse().unwrap());
        request.headers_mut().insert(
            "x-aistudio-worker-id",
            "aistudio-inventory".parse().unwrap(),
        );
        let (mut socket, response) = bounded(tokio_tungstenite::connect_async(request))
            .await
            .expect("worker upgrade");
        assert_eq!(response.status().as_u16(), 101);
        let sent =
            json!({"id":"inventory-catalog", "type":"catalog", "payload":{"models":[],"ack":true}})
                .to_string();
        bounded(socket.send(Message::Text(sent.clone().into())))
            .await
            .expect("worker catalog");
        let received = bounded(socket.next())
            .await
            .expect("worker acknowledgement")
            .expect("worker frame");
        let text = received.to_text().expect("worker text").to_owned();
        let value: Value = serde_json::from_str(&text).expect("worker acknowledgement JSON");
        assert_eq!(value["id"], "inventory-catalog");
        assert_eq!(value["type"], "catalog_ack");
        bounded(socket.close(None)).await.expect("worker close");
        json!({"path":"/v1/ws", "status":101, "sent_utf8":sent, "received_utf8":text})
    };
    for path in [
        "/backend-api/codex/responses",
        "/v1/responses",
        "/responses",
        "/v1/realtime",
    ] {
        let mut capture =
            websocket_capture(format!("ws://{address}{path}"), "inventory-owner").await;
        capture["path"] = json!(path);
        capture["upstream"] = bounded(observed.recv())
            .await
            .expect("upstream upgraded event");
        assert_eq!(capture["upstream"]["kind"], "upgrade");
        captures.push(capture);
    }
    let http = reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("loopback client");
    for (offer_path, sideband_path) in [
        ("/v1/live", "/v1/live/"),
        ("/live", "/live/"),
        ("/v1/realtime", "/v1/realtime/calls/"),
        ("/v1/realtime/calls", "/v1/realtime/calls/"),
    ] {
        let peer = media_peer().await;
        peer.create_data_channel("oai-events", None)
            .await
            .expect("client data channel");
        let offer = local_sdp(&peer, true).await;
        let response = bounded(
            http.post(format!("http://{address}{offer_path}"))
                .bearer_auth("inventory-owner")
                .json(&json!({"sdp":offer,"session":{}}))
                .send(),
        )
        .await
        .expect("call offer");
        assert_eq!(response.status().as_u16(), 201);
        let location = response.headers()["location"].to_str().unwrap().to_owned();
        let answer = bounded(response.text()).await.expect("client answer");
        peer.set_remote_description(
            RTCSessionDescription::answer(answer.clone()).expect("answer SDP"),
        )
        .await
        .expect("client accepts answer");
        let upstream_offer = bounded(observed.recv())
            .await
            .expect("upstream offer event");
        assert_eq!(upstream_offer["kind"], "offer");
        assert_eq!(upstream_offer["location"], location);
        let call_id = location.rsplit('/').next().unwrap();
        let path = format!("{sideband_path}{call_id}");
        let denied = bounded(
            http.get(format!("http://{address}{path}"))
                .bearer_auth("inventory-other")
                .send(),
        )
        .await
        .expect("non-owner request");
        assert_eq!(denied.status().as_u16(), 403);
        let mut capture =
            websocket_capture(format!("ws://{address}{path}"), "inventory-owner").await;
        capture["path"] = json!(path);
        capture["offer_path"] = json!(offer_path);
        capture["offer_status"] = json!(201);
        capture["non_owner_status"] = json!(403);
        capture["location"] = json!(location);
        capture["offer_sdp"] = json!(offer);
        capture["answer_sdp"] = json!(answer);
        capture["upstream_offer"] = upstream_offer;
        capture["upstream"] = bounded(observed.recv())
            .await
            .expect("sideband upgraded event");
        assert_eq!(capture["upstream"]["path"], location);
        let hangup = bounded(
            http.post(format!("http://{address}{location}/hangup"))
                .bearer_auth("inventory-owner")
                .send(),
        )
        .await
        .expect("owned hangup");
        assert_eq!(hangup.status().as_u16(), 200);
        bounded(peer.close()).await.expect("client peer close");
        captures.push(capture);
    }
    for peer in upstream.peers.lock().await.drain(..) {
        bounded(peer.close()).await.expect("upstream peer close");
    }
    gateway_task.close().await;
    upstream_task.close().await;
    json!({"captures":captures,"worker_capture":worker_capture})
}

struct FixtureDirectory(std::path::PathBuf);
impl Drop for FixtureDirectory {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            panic!("inventory fixture cleanup {}: {error}", self.0.display());
        }
    }
}

/// Populate fields omitted by default serialization as well as dynamic containers.
/// This is a typed Settings instance, serialized by the real serde implementation.
fn populated_settings() -> Settings {
    let mut settings = Settings {
        github_token: "inventory-fixture-not-a-token".into(),
        auth_dir: "inventory-fixture-auth".into(),
        ..Settings::default()
    };
    settings.remote_management.secret_key = "inventory-fixture-management".into();
    settings.api_keys = vec!["inventory-fixture-client".into()];
    settings.routing.session_affinity = Some(true);
    settings.routing.session_affinity_lcp = true;
    settings.routing.session_affinity_ttl = "1h".into();
    settings.routing.lcp_max_turns = Some(3);
    settings.routing.lcp_max_groups = Some(4);
    settings.routing.lcp_max_prefixes = Some(5);
    settings.model_catalog = Some(ModelCatalogSettings {
        refresh_enabled: false,
        allowed_blackouts: vec!["inventory-fixture-provider".into()],
        ..Default::default()
    });
    settings.omo_profile = Some(OmoProfile::default());
    settings.credit_codex_account_ids = vec!["inventory-fixture-account".into()];
    settings.oauth_model_alias = json!({"inventory-fixture": []});
    settings.oauth_request_scoped_errors = json!({"inventory-fixture": []});
    settings
        .claude_code
        .extra
        .insert("inventory-fixture".into(), json!(true));
    settings
        .codex
        .extra
        .insert("inventory-fixture".into(), json!(true));
    settings
        .antigravity
        .extra
        .insert("inventory-fixture".into(), json!(true));
    settings
        .xai
        .extra
        .insert("inventory-fixture".into(), json!(true));
    settings.tls.extra.insert("enable".into(), json!(false));
    settings
        .streaming
        .extra
        .insert("keepalive-seconds".into(), json!(0));
    settings.pprof.extra.insert("enable".into(), json!(false));
    settings.extra.insert(
        serde_yaml::Value::String("inventory-fixture-extra".into()),
        serde_yaml::Value::Bool(true),
    );
    settings
}

fn leaf_paths(value: &Value, prefix: &str, output: &mut Vec<String>) {
    match value {
        Value::Object(fields) if !fields.is_empty() => {
            for (key, child) in fields {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                leaf_paths(child, &format!("{prefix}/{escaped}"), output);
            }
        }
        // Arrays are declared collection settings. Their full value is read
        // back; indices are not fabricated source-level configuration keys.
        _ => output.push(prefix.to_owned()),
    }
}

/// Returns real serialized fixture/config bytes, not a manually assembled view.
/// The final isolated QA runner may persist this value with its build provenance.
async fn settings_inventory_fixture() -> Result<Value, Box<dyn std::error::Error>> {
    let path = std::env::temp_dir().join(format!("inventory-qa-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&path)?;
    let dir = FixtureDirectory(path);
    let auth = dir.0.join("auth");
    std::fs::create_dir(&auth)?;
    let mut settings = populated_settings();
    settings.auth_dir = auth.to_string_lossy().into_owned();
    let yaml = serde_yaml::to_string(&settings)?;
    let config_path = dir.0.join("config.yaml");
    std::fs::write(&config_path, &yaml)?;
    let state = Arc::new(crate::state::AppState::new(
        &crate::config::GatewayConfig {
            auth_dir: auth,
            config_path,
            auth_refresh_enabled: false,
            catalog_cache_path: Some(dir.0.join("catalog-cache")),
            ..Default::default()
        },
    )?);
    let app = crate::routes::create_app(state.clone());
    let mut captures = Vec::new();
    for path in ["/v0/management/config", "/v8/management/config"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header("x-management-key", "inventory-fixture-management")
                    .extension(ConnectInfo(
                        "127.0.0.1:18840".parse::<std::net::SocketAddr>()?,
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await?;
        if status != 200 {
            return Err(format!(
                "{path} returned {status}: {}",
                String::from_utf8_lossy(&bytes)
            )
            .into());
        }
        let text = String::from_utf8(bytes.to_vec())?;
        let value: Value = serde_json::from_str(&text)?;
        captures.push(
            json!({"method":"GET", "path":path, "status":status, "body_utf8":text, "value":value}),
        );
    }
    let v8 = captures[1]["value"].clone();
    let mut paths = Vec::new();
    leaf_paths(&v8, "", &mut paths);
    let mut readbacks = Vec::new();
    for pointer in paths {
        // JSON pointers are decoded to path components and URI encoded. A
        // literal slash inside a key cannot be represented by this route;
        // record no successful readback for such a key.
        let mut components = Vec::new();
        for key in pointer.trim_start_matches('/').split('/') {
            let decoded = key.replace("~1", "/").replace("~0", "~");
            if decoded.contains('/') {
                return Err("v8 field key contains unaddressable slash".into());
            }
            let encoded = decoded
                .bytes()
                .map(|byte| {
                    if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                        (byte as char).to_string()
                    } else {
                        format!("%{byte:02X}")
                    }
                })
                .collect::<String>();
            components.push(encoded);
        }
        let path = format!("/v8/management/config/{}", components.join("/"));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(&path)
                    .header("x-management-key", "inventory-fixture-management")
                    .extension(ConnectInfo(
                        "127.0.0.1:18840".parse::<std::net::SocketAddr>()?,
                    ))
                    .body(Body::empty())?,
            )
            .await?;
        let status = response.status().as_u16();
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024).await?;
        if status != 200 {
            return Err(format!("leaf {pointer}: HTTP {status}").into());
        }
        let text = String::from_utf8(bytes.to_vec())?;
        let value: Value = serde_json::from_str(&text)?;
        if Some(&value) != v8.pointer(&pointer) {
            return Err(format!("leaf readback mismatch {pointer}").into());
        }
        readbacks.push(
            json!({"pointer":pointer,"method":"GET","path":path,"status":status,"body_utf8":text}),
        );
    }
    // oneshot returns after the actual handler has subscribed. Publish only
    // then, and await a data frame rather than a timer or an SSE keepalive.
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/v0/management/logs/stream")
                .header("x-management-key", "inventory-fixture-management")
                .extension(ConnectInfo(
                    "127.0.0.1:18840".parse::<std::net::SocketAddr>()?,
                ))
                .body(Body::empty())?,
        )
        .await?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .ok_or("missing SSE content type")?
        .to_str()?
        .to_owned();
    let trigger = json!({"kind":"proxy", "message":"inventory-subscribed-frame"});
    state.log_tail.push(trigger.to_string());
    use http_body_util::BodyExt;
    let mut body = response.into_body();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(5), body.frame())
        .await?
        .ok_or("missing SSE frame")??;
    let bytes = frame
        .into_data()
        .map_err(|_| "SSE trailer instead of data")?;
    let text = String::from_utf8(bytes.to_vec())?;
    let data = text
        .strip_prefix("data: ")
        .or_else(|| text.strip_prefix("data:"))
        .ok_or("SSE data field missing")?
        .trim();
    if status != 200 || serde_json::from_str::<Value>(data)? != trigger {
        return Err("SSE subscribed frame mismatch".into());
    }
    drop(body);
    let stream_capture = json!({
        "method":"GET", "path":"/v0/management/logs/stream",
        "status":status, "content_type":content_type,
        "body_utf8":text, "trigger":trigger,
    });
    let mut route_captures = Vec::new();
    for scalar in super::scalar_table::SCALARS {
        let path = format!("/v0/management{}", scalar.path);
        for method in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"] {
            if method == "DELETE" && scalar.clear.is_none() {
                continue;
            }
            let channel = super::scalar_table::is_channel_keyed(scalar.path);
            let (request_body, status, expected) = match method {
                "GET" | "HEAD" => (
                    "",
                    200,
                    json!({scalar.key:(scalar.read)(&state.settings.current())}),
                ),
                "PATCH" if channel => ("!", 400, json!({"error":"invalid channel"})),
                "DELETE" if channel => ("", 400, json!({"error":"missing channel"})),
                "DELETE" => ("", 200, json!({"status":"ok"})),
                _ => ("!", 400, json!({"error":"invalid body"})),
            };
            route_captures.push(
                capture_json_route(&app, method, &path, request_body, status, &expected).await?,
            );
        }
    }
    for path in super::apikeys::key_lists_paths() {
        let key = path.trim_start_matches('/');
        let serialized = serde_json::to_value(state.settings.current().as_ref())?;
        let expected = json!({key: serialized.get(key).ok_or("missing key-list setting")?});
        let path = format!("/v0/management{path}");
        for method in ["GET", "HEAD"] {
            route_captures.push(capture_json_route(&app, method, &path, "", 200, &expected).await?);
        }
        for method in ["POST", "PUT", "PATCH", "DELETE"] {
            let (body, expected) = if method == "DELETE" {
                ("", json!({"error":"missing index or value"}))
            } else {
                ("!", json!({"error":"invalid body"}))
            };
            route_captures
                .push(capture_json_route(&app, method, &path, body, 400, &expected).await?);
        }
    }
    Ok(json!({
        "schema":"inventory-native-settings/v1",
        "run_id": uuid::Uuid::new_v4().to_string(),
        "serialized_settings": serde_json::to_value(&settings)?,
        "config_yaml": yaml,
        "captures": captures,
        "leaf_readbacks": readbacks,
        "stream_capture": stream_capture,
        "route_captures": route_captures,
    }))
}

async fn capture_json_route(
    app: &axum::Router,
    method: &str,
    path: &str,
    request_body: &str,
    expected_status: u16,
    expected: &Value,
) -> Result<Value, Box<dyn std::error::Error>> {
    let response = bounded(
        app.clone().oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-management-key", "inventory-fixture-management")
                .header(
                    "authorization",
                    if path.starts_with("/v0/management/") || path.starts_with("/v8/management/") {
                        "Bearer inventory-fixture-management"
                    } else {
                        "Bearer inventory-fixture-client"
                    },
                )
                .header("content-type", "application/json")
                .extension(ConnectInfo(
                    "127.0.0.1:18840".parse::<std::net::SocketAddr>()?,
                ))
                .body(Body::from(request_body.to_owned()))?,
        ),
    )
    .await?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .ok_or("route content type")?
        .to_str()?
        .to_owned();
    let bytes = bounded(to_bytes(response.into_body(), 4 * 1024 * 1024)).await?;
    let body = String::from_utf8(bytes.to_vec())?;
    if status != expected_status {
        return Err(
            format!("{method} {path}: expected {expected_status}, got {status}: {body}").into(),
        );
    }
    if method == "HEAD" {
        if !body.is_empty() {
            return Err("HEAD returned bytes".into());
        }
    } else if &serde_json::from_str::<Value>(&body)? != expected {
        return Err(
            format!("{method} {path}: response differs from handler contract: {body}").into(),
        );
    }
    Ok(
        json!({"method":method,"path":path,"request_body_utf8":request_body,
        "status":status,"content_type":content_type,"body_utf8":body}),
    )
}

#[tokio::test]
async fn inventory_fixture_uses_actual_config_handlers() {
    const CHILD: &str = "MAHOQUOT_INVENTORY_FIXTURE_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = bounded(
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "management::inventory_qa::inventory_fixture_uses_actual_config_handlers",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("MAHOQUOT_QA_LOOPBACK_ONLY", "1")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        print!("{}", String::from_utf8_lossy(&output.stdout));
        return;
    }
    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .expect("isolated fixture crypto provider");
    let mut fixture = settings_inventory_fixture()
        .await
        .expect("native inventory fixture");
    fixture["protocols"] = protocol_inventory_fixture().await;
    let captures = fixture["captures"].as_array().expect("captures");
    assert_eq!(captures.len(), 2);
    let v0 = &captures[0]["value"];
    assert!(v0.get("remote-management").is_none());
    assert!(v0.get("home").is_none());
    assert_eq!(
        v0["request-retry"],
        fixture["serialized_settings"]["request-retry"]
    );
    assert!(!captures[1]["body_utf8"]
        .as_str()
        .unwrap()
        .contains("inventory-fixture-management"));
    for capture in captures {
        assert_eq!(
            serde_json::from_str::<Value>(capture["body_utf8"].as_str().unwrap()).unwrap(),
            capture["value"]
        );
    }
    for row in &fixture["leaf_readbacks"]
        .as_array()
        .expect("leaf readbacks")[..]
    {
        let value: Value = serde_json::from_str(row["body_utf8"].as_str().expect("leaf bytes"))
            .expect("leaf JSON");
        assert_eq!(
            Some(&value),
            captures[1]["value"].pointer(row["pointer"].as_str().unwrap())
        );
    }
    // Dedicated test output is consumed only with a source-bound test-run
    // receipt. Never read a caller-provided output path or overwrite a file.
    println!(
        "INVENTORY_NATIVE_SETTINGS_V1={}",
        serde_json::to_string(&fixture).unwrap()
    );
}

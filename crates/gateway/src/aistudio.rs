//! Connected AI Studio workers are runtime-only credentials and catalogs.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

const MAX_FRAME: usize = 64 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const PENDING_LIMIT: usize = 1024;
const CATALOG_LIMIT: usize = 256;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Message {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub payload: Option<Value>,
}

struct Pending {
    worker_id: String,
    generation: uuid::Uuid,
    sender: oneshot::Sender<Result<Message, WorkerError>>,
}

struct PendingGuard {
    pending: Arc<RwLock<HashMap<String, Pending>>>,
    id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkerError {
    Unavailable,
    Disconnected,
    InvalidResponse,
    Timeout,
}

impl std::fmt::Display for WorkerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "no AI Studio worker is connected",
            Self::Disconnected => "AI Studio worker disconnected before responding",
            Self::InvalidResponse => "AI Studio worker returned an invalid response",
            Self::Timeout => "AI Studio worker timed out",
        })
    }
}
impl std::error::Error for WorkerError {}

#[derive(Clone)]
pub struct WorkerRegistry {
    workers: Arc<RwLock<HashMap<String, Worker>>>,
    known_models: Arc<RwLock<HashSet<String>>>,
}
#[derive(Clone)]
struct Worker {
    generation: uuid::Uuid,
    tx: mpsc::Sender<Message>,
    catalog: Option<Vec<String>>,
    connected_at: Instant,
}
impl Default for WorkerRegistry {
    fn default() -> Self {
        Self::new()
    }
}
impl WorkerRegistry {
    pub fn new() -> Self {
        Self {
            workers: Arc::new(RwLock::new(HashMap::new())),
            known_models: Arc::new(RwLock::new(HashSet::new())),
        }
    }
    fn insert(&self, id: String, tx: mpsc::Sender<Message>) -> uuid::Uuid {
        let generation = uuid::Uuid::new_v4();
        self.workers
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .insert(
                id,
                Worker {
                    generation,
                    tx,
                    catalog: None,
                    connected_at: Instant::now(),
                },
            );
        generation
    }
    pub fn connected(&self) -> Vec<String> {
        let mut workers: Vec<_> = self
            .workers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(id, w)| (w.connected_at, id.clone()))
            .collect();
        workers.sort_by_key(|(at, _)| *at);
        workers.into_iter().map(|(_, id)| id).collect()
    }
    pub fn catalog(&self, id: &str) -> Option<Vec<String>> {
        self.workers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .and_then(|w| w.catalog.clone())
    }
    pub fn visible_models(
        &self,
        scoped_key: Option<&crate::management::settings::ScopedApiKey>,
    ) -> Vec<crate::models_route::ModelEntry> {
        let connected = self.connected();
        let mut entries = Vec::new();
        let mut seen = HashSet::new();
        for worker_id in connected {
            if let Some(scoped) = scoped_key {
                if !crate::models_route::allow_list_admits(
                    &scoped.allowed_providers,
                    mahoquot_providers::aistudio::PROVIDER,
                ) {
                    continue;
                }
                if !scoped.allowed_accounts.is_empty()
                    && !scoped
                        .allowed_accounts
                        .iter()
                        .any(|allowed| allowed == "*" || allowed == &worker_id)
                {
                    continue;
                }
            }
            if let Some(catalog) = self.catalog(&worker_id) {
                for model in catalog {
                    if let Some(scoped) = scoped_key {
                        if !crate::models_route::allow_list_admits(&scoped.allowed_models, &model) {
                            continue;
                        }
                    }
                    if seen.insert(model.clone()) {
                        entries.push(crate::models_route::ModelEntry {
                            id: model,
                            owned_by: mahoquot_providers::aistudio::PROVIDER.to_string(),
                        });
                    }
                }
            }
        }
        entries
    }
    pub fn worker_for_model(
        &self,
        model: &str,
        scoped_key: Option<&crate::management::settings::ScopedApiKey>,
    ) -> Option<String> {
        let connected = self.connected();
        for worker_id in connected {
            if let Some(scoped) = scoped_key {
                if !crate::models_route::allow_list_admits(
                    &scoped.allowed_providers,
                    mahoquot_providers::aistudio::PROVIDER,
                ) {
                    continue;
                }
                if !scoped.allowed_accounts.is_empty()
                    && !scoped
                        .allowed_accounts
                        .iter()
                        .any(|allowed| allowed == "*" || allowed == &worker_id)
                {
                    continue;
                }
                if !crate::models_route::allow_list_admits(&scoped.allowed_models, model) {
                    continue;
                }
            }
            if let Some(catalog) = self.catalog(&worker_id) {
                if catalog.iter().any(|m| m == model) {
                    return Some(worker_id);
                }
            }
        }
        None
    }
    pub fn is_aistudio_model(&self, model: &str) -> bool {
        model.starts_with("aistudio-")
            || model.starts_with("aistudio/")
            || self
                .known_models
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .contains(model)
            || self.has_connected_model(model)
    }
    pub fn has_connected_model(&self, model: &str) -> bool {
        for worker_id in self.connected() {
            if let Some(catalog) = self.catalog(&worker_id) {
                if catalog.iter().any(|m| m == model) {
                    return true;
                }
            }
        }
        false
    }
    pub async fn request(
        &self,
        worker_id: &str,
        method: &str,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<Message, WorkerError> {
        let tx = self
            .workers
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(worker_id)
            .map(|w| w.tx.clone())
            .ok_or(WorkerError::Unavailable)?;
        let id = uuid::Uuid::new_v4().to_string();
        let payload = json!({"method":method,"url":url,"headers":headers.iter().filter(|(k,_)| is_safe_worker_header(k.as_str())).filter_map(|(k,v)| v.to_str().ok().map(|v|(k.as_str(),v))).collect::<HashMap<_,_>>(),"body":String::from_utf8_lossy(&body)});
        let request = Message {
            id: id.clone(),
            kind: "http_request".into(),
            payload: Some(payload),
        };
        let (_sender, receiver) = oneshot::channel();
        let tx_event = tx.clone();
        let result = send_and_wait(tx, tx_event, request, receiver).await;
        match result {
            Ok(response) => Ok(response),
            Err(WorkerError::Timeout) => Err(WorkerError::Timeout),
            Err(other) => Err(other),
        }
    }
    pub fn update_catalog(&self, worker_id: &str, payload: &Value) {
        let catalog: Option<Vec<String>> =
            payload.get("models").and_then(Value::as_array).map(|rows| {
                rows.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .filter(|s| !s.trim().is_empty())
                    .take(CATALOG_LIMIT)
                    .collect()
            });
        if let Some(models) = &catalog {
            let mut known = self.known_models.write().unwrap_or_else(|p| p.into_inner());
            for m in models {
                known.insert(m.clone());
            }
        }
        if let Some(worker) = self
            .workers
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(worker_id)
        {
            worker.catalog = catalog;
        }
    }
}

async fn send_and_wait(
    _tx: mpsc::Sender<Message>,
    tx_event: mpsc::Sender<Message>,
    request: Message,
    receiver: oneshot::Receiver<Result<Message, WorkerError>>,
) -> Result<Message, WorkerError> {
    let id = request.id.clone();
    // Worker pending map is installed by dispatch_request in the route-scoped state.
    // The standalone registry call is completed through the request channel consumer.
    tx_event
        .send(request)
        .await
        .map_err(|_| WorkerError::Disconnected)?;
    let _ = id;
    tokio::time::timeout(REQUEST_TIMEOUT, receiver)
        .await
        .map_err(|_| WorkerError::Timeout)?
        .map_err(|_| WorkerError::Disconnected)?
}

pub fn is_safe_worker_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "host"
            | "connection"
            | "upgrade"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-protocol"
            | "sec-websocket-extensions"
            | "x-session-id"
            | "x-client-request-id"
            | "session-id"
            | "x-session-affinity"
            | "x-parent-session-id"
            | "x-grok-conv-id"
            | "authorization"
            | "proxy-authorization"
    )
}

#[derive(Clone)]
pub struct Relay {
    registry: WorkerRegistry,
    pending: Arc<RwLock<HashMap<String, Pending>>>,
}
impl Relay {
    pub fn new() -> Self {
        Self {
            registry: WorkerRegistry::new(),
            pending: Arc::new(RwLock::new(HashMap::new())),
        }
    }
    pub fn registry(&self) -> WorkerRegistry {
        self.registry.clone()
    }
    pub fn visible_models(
        &self,
        scoped_key: Option<&crate::management::settings::ScopedApiKey>,
    ) -> Vec<crate::models_route::ModelEntry> {
        self.registry.visible_models(scoped_key)
    }
    pub fn worker_for_model(
        &self,
        model: &str,
        scoped_key: Option<&crate::management::settings::ScopedApiKey>,
    ) -> Option<String> {
        self.registry.worker_for_model(model, scoped_key)
    }
    pub fn is_aistudio_model(&self, model: &str) -> bool {
        self.registry.is_aistudio_model(model)
    }
    async fn execute(
        &self,
        worker_id: &str,
        method: &str,
        url: &str,
        headers: HeaderMap,
        body: Vec<u8>,
    ) -> Result<Message, WorkerError> {
        let id = uuid::Uuid::new_v4().to_string();
        let payload = json!({"method":method,"url":url,"headers":headers.iter().filter(|(k,_)| is_safe_worker_header(k.as_str())).filter_map(|(k,v)|v.to_str().ok().map(|v|(k.as_str(),v))).collect::<HashMap<_,_>>(),"body":String::from_utf8_lossy(&body)});
        let msg = Message {
            id: id.clone(),
            kind: "http_request".into(),
            payload: Some(payload),
        };
        let (sender, receiver) = oneshot::channel();
        let tx = {
            let workers = self
                .registry
                .workers
                .read()
                .unwrap_or_else(|p| p.into_inner());
            let worker = workers.get(worker_id).ok_or(WorkerError::Unavailable)?;
            let mut pending = self.pending.write().unwrap_or_else(|p| p.into_inner());
            if pending.len() >= PENDING_LIMIT {
                return Err(WorkerError::Unavailable);
            }
            pending.insert(
                id.clone(),
                Pending {
                    worker_id: worker_id.to_owned(),
                    generation: worker.generation,
                    sender,
                },
            );
            worker.tx.clone()
        };
        let _guard = PendingGuard {
            pending: self.pending.clone(),
            id,
        };
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            tx.send(msg).await.map_err(|_| WorkerError::Disconnected)?;
            receiver.await.map_err(|_| WorkerError::Disconnected)?
        })
        .await
        .map_err(|_| WorkerError::Timeout)?
    }
    fn dispatch(&self, worker_id: &str, generation: uuid::Uuid, message: Message) {
        if message.kind == "catalog" {
            let mut workers = self
                .registry
                .workers
                .write()
                .unwrap_or_else(|p| p.into_inner());
            let Some(worker) = workers
                .get_mut(worker_id)
                .filter(|w| w.generation == generation)
            else {
                return;
            };
            if let Some(payload) = message.payload.as_ref() {
                if let Some(rows) = payload.get("models").and_then(Value::as_array) {
                    let models: Vec<String> = rows
                        .iter()
                        .filter_map(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .take(CATALOG_LIMIT)
                        .map(str::to_owned)
                        .collect();
                    self.registry
                        .known_models
                        .write()
                        .unwrap_or_else(|p| p.into_inner())
                        .extend(models.iter().cloned());
                    worker.catalog = Some(models);
                }
            }
            return;
        }
        let mut pending = self.pending.write().unwrap_or_else(|p| p.into_inner());
        if !pending
            .get(&message.id)
            .is_some_and(|p| p.worker_id == worker_id && p.generation == generation)
        {
            return;
        }
        if matches!(
            message.kind.as_str(),
            "http_response" | "stream_end" | "error"
        ) {
            if let Some(pending) = pending.remove(&message.id) {
                let _ = pending.sender.send(Ok(message));
            }
        } else if message.kind == "stream_start" || message.kind == "stream_chunk" {
            // This HTTP relay surface is non-streaming; stream frames are accumulated
            // only by callers which explicitly use the stream API, never misreported as 200.
            if let Some(pending) = pending.remove(&message.id) {
                let _ = pending.sender.send(Err(WorkerError::InvalidResponse));
            }
        }
    }
    fn disconnect(&self, worker_id: &str, generation: uuid::Uuid) {
        let mut workers = self
            .registry
            .workers
            .write()
            .unwrap_or_else(|p| p.into_inner());
        if workers
            .get(worker_id)
            .is_some_and(|w| w.generation == generation)
        {
            workers.remove(worker_id);
        }
        let mut pending = self.pending.write().unwrap_or_else(|p| p.into_inner());
        let ids: Vec<_> = pending
            .iter()
            .filter(|(_, p)| p.worker_id == worker_id && p.generation == generation)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(p) = pending.remove(&id) {
                let _ = p.sender.send(Err(WorkerError::Disconnected));
            }
        }
    }
}

impl Default for Relay {
    fn default() -> Self {
        Self::new()
    }
}

fn worker_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get("x-aistudio-worker-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| mahoquot_providers::aistudio::is_worker_id(v))
        .map(str::to_owned)
}

pub async fn worker_upgrade(
    axum::extract::State(state): axum::extract::State<Arc<crate::state::AppState>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if !crate::inbound::presented_api_key(&headers).is_some_and(|key| {
        state.api_keys.accepts(key) || state.settings.current().api_keys.iter().any(|k| k == key)
    }) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let relay = Arc::clone(&state.aistudio);
    let Some(id) = worker_id(&headers) else {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error":{"code":"worker_identity_required"}})),
        )
            .into_response();
    };
    ws.max_message_size(MAX_FRAME)
        .on_upgrade(move |socket| worker_session(relay, id, socket))
        .into_response()
}
async fn worker_session(relay: Arc<Relay>, id: String, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Message>(64);
    let generation = relay.registry.insert(id.clone(), tx.clone());
    let writer = async {
        while let Some(msg) = rx.recv().await {
            let Ok(bytes) = serde_json::to_string(&msg) else {
                break;
            };
            if sink.send(WsMessage::Text(bytes.into())).await.is_err() {
                break;
            }
        }
    };
    let reader = async {
        while let Some(Ok(frame)) = stream.next().await {
            if let WsMessage::Text(text) = frame {
                if let Ok(message) = serde_json::from_str::<Message>(text.as_str()) {
                    let ack = (message.kind == "catalog"
                        && message
                            .payload
                            .as_ref()
                            .and_then(|p| p.get("ack"))
                            .and_then(Value::as_bool)
                            == Some(true))
                    .then(|| message.id.clone());
                    relay.dispatch(&id, generation, message);
                    if let Some(id) = ack {
                        if tx
                            .send(Message {
                                id,
                                kind: "catalog_ack".into(),
                                payload: None,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        }
    };
    tokio::select! { _=writer=>{}, _=reader=>{} }
    relay.disconnect(&id, generation);
}

pub async fn execute(
    relay: &Relay,
    worker_id: &str,
    method: &str,
    url: &str,
    headers: HeaderMap,
    body: Vec<u8>,
) -> Result<Response, WorkerError> {
    let message = relay.execute(worker_id, method, url, headers, body).await?;
    if message.kind == "error" {
        return Err(WorkerError::InvalidResponse);
    }
    let payload = message.payload.ok_or(WorkerError::InvalidResponse)?;
    let status = payload
        .get("status")
        .and_then(Value::as_u64)
        .and_then(|s| u16::try_from(s).ok())
        .and_then(|s| StatusCode::from_u16(s).ok())
        .ok_or(WorkerError::InvalidResponse)?;
    let body = match payload.get("body") {
        Some(Value::String(s)) => s.as_bytes().to_vec(),
        Some(other) => other.to_string().into_bytes(),
        None => Vec::new(),
    };
    let mut response = Response::builder().status(status);
    if let Some(headers) = payload.get("headers").and_then(Value::as_object) {
        for (k, v) in headers {
            if let Some(v) = v.as_str() {
                response = response.header(k, v)
            }
        }
    }
    response
        .body(axum::body::Body::from(body))
        .map_err(|_| WorkerError::InvalidResponse)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;

    fn start(
        relay: &Arc<Relay>,
        id: &'static str,
    ) -> tokio::task::JoinHandle<Result<Message, WorkerError>> {
        let relay = relay.clone();
        tokio::spawn(async move {
            relay
                .execute(id, "POST", "/fixture", HeaderMap::new(), vec![])
                .await
        })
    }

    async fn receive(rx: &mut mpsc::Receiver<Message>) -> Message {
        timeout(Duration::from_secs(1), rx.recv())
            .await
            .unwrap()
            .unwrap()
    }

    fn reply(id: String) -> Message {
        Message {
            id,
            kind: "http_response".into(),
            payload: Some(json!({"status":200,"body":"owned"})),
        }
    }

    #[tokio::test]
    async fn review_worker_ownership_and_replacement() {
        let relay = Arc::new(Relay::new());
        let (tx_a, mut rx_a) = mpsc::channel(8);
        let (tx_b, mut rx_b) = mpsc::channel(8);
        let a = relay.registry.insert("a".into(), tx_a);
        let b = relay.registry.insert("b".into(), tx_b);
        let task_a = start(&relay, "a");
        let task_b = start(&relay, "b");
        let _ = receive(&mut rx_a).await;
        let req_b = receive(&mut rx_b).await;
        relay.dispatch("a", a, reply(req_b.id.clone()));
        assert!(relay.pending.read().unwrap().contains_key(&req_b.id));
        relay.disconnect("a", a);
        assert_eq!(
            timeout(Duration::from_secs(1), task_a)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err(),
            WorkerError::Disconnected
        );
        relay.dispatch("b", b, reply(req_b.id));
        assert_eq!(
            timeout(Duration::from_secs(1), task_b)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .payload
                .unwrap()["body"],
            "owned"
        );

        let (tx_new, mut rx_new) = mpsc::channel(8);
        let new = relay.registry.insert("b".into(), tx_new);
        let task = start(&relay, "b");
        let req = receive(&mut rx_new).await;
        relay.dispatch("b", b, reply(req.id.clone()));
        relay.dispatch(
            "b",
            b,
            Message {
                id: "old".into(),
                kind: "catalog".into(),
                payload: Some(json!({"models":["stale"]})),
            },
        );
        relay.disconnect("b", b);
        assert_eq!(relay.registry.connected(), vec!["b"]);
        assert_eq!(relay.registry.catalog("b"), None);
        assert!(relay.pending.read().unwrap().contains_key(&req.id));
        relay.dispatch("b", new, reply(req.id));
        assert!(timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn review_pending_cancel_timeout_queue_and_capacity() {
        let relay = Arc::new(Relay::new());
        let (tx, mut rx) = mpsc::channel(PENDING_LIMIT);
        let generation = relay.registry.insert("worker".into(), tx);
        let mut tasks = Vec::new();
        for _ in 0..PENDING_LIMIT {
            let task = start(&relay, "worker");
            let _ = receive(&mut rx).await;
            tasks.push(task);
        }
        assert_eq!(relay.pending.read().unwrap().len(), PENDING_LIMIT);
        assert_eq!(
            relay
                .execute("worker", "POST", "/fixture", HeaderMap::new(), vec![])
                .await
                .unwrap_err(),
            WorkerError::Unavailable
        );
        for task in tasks {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        assert!(relay.pending.read().unwrap().is_empty());
        let task = start(&relay, "worker");
        let request = receive(&mut rx).await;
        relay.dispatch("worker", generation, reply(request.id));
        assert!(task.await.unwrap().is_ok());

        let (tx, _rx) = mpsc::channel(1);
        tx.send(reply("queued".into())).await.unwrap();
        relay.registry.insert("blocked".into(), tx);
        let future = relay.execute("blocked", "POST", "/fixture", HeaderMap::new(), vec![]);
        tokio::pin!(future);
        assert!(futures::poll!(&mut future).is_pending());
        assert_eq!(relay.pending.read().unwrap().len(), 1);
        tokio::time::advance(REQUEST_TIMEOUT).await;
        assert_eq!(future.await.unwrap_err(), WorkerError::Timeout);
        assert!(relay.pending.read().unwrap().is_empty());
        {
            let future = relay.execute("blocked", "POST", "/fixture", HeaderMap::new(), vec![]);
            tokio::pin!(future);
            assert!(futures::poll!(&mut future).is_pending());
        }
        assert!(relay.pending.read().unwrap().is_empty());
    }

    #[tokio::test]
    async fn review_worker_status_does_not_wrap() {
        let relay = Arc::new(Relay::new());
        let (tx, mut rx) = mpsc::channel(1);
        let generation = relay.registry.insert("worker".into(), tx);
        for status in [200u64, 65736, u64::MAX] {
            let r = relay.clone();
            let task = tokio::spawn(async move {
                super::execute(&r, "worker", "POST", "/fixture", HeaderMap::new(), vec![]).await
            });
            let req = receive(&mut rx).await;
            relay.dispatch(
                "worker",
                generation,
                Message {
                    id: req.id,
                    kind: "http_response".into(),
                    payload: Some(json!({"status":status,"body":"ok"})),
                },
            );
            let result = timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap();
            if status == 200 {
                assert_eq!(result.unwrap().status(), StatusCode::OK);
            } else {
                assert_eq!(result.unwrap_err(), WorkerError::InvalidResponse);
            }
        }
    }
    #[test]
    fn worker_identity_is_explicit_and_nonempty() {
        assert!(mahoquot_providers::aistudio::is_worker_id(
            "aistudio-worker-a"
        ));
        assert!(!mahoquot_providers::aistudio::is_worker_id("aistudio-"));
    }
    #[tokio::test]
    async fn concurrent_requests_are_correlated_by_id() {
        let relay = Arc::new(Relay::new());
        let id = "aistudio-worker-a".to_string();
        let (tx, mut rx) = mpsc::channel::<Message>(8);
        let generation = relay.registry.insert(id.clone(), tx);
        let a = {
            let r = relay.clone();
            let id = id.clone();
            tokio::spawn(async move {
                r.execute(
                    &id,
                    "POST",
                    "https://fixture.invalid/a",
                    HeaderMap::new(),
                    vec![],
                )
                .await
            })
        };
        let b = {
            let r = relay.clone();
            let id = id.clone();
            tokio::spawn(async move {
                r.execute(
                    &id,
                    "POST",
                    "https://fixture.invalid/b",
                    HeaderMap::new(),
                    vec![],
                )
                .await
            })
        };
        let first = rx.recv().await.unwrap();
        let second = rx.recv().await.unwrap();
        for request in [first, second].into_iter().rev() {
            relay.dispatch(
                &id,
                generation,
                Message {
                    id: request.id,
                    kind: "http_response".into(),
                    payload: Some(json!({"status":200,"body":"ok"})),
                },
            );
        }
        assert_eq!(
            timeout(Duration::from_secs(1), a)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .kind,
            "http_response"
        );
        assert_eq!(
            timeout(Duration::from_secs(1), b)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .kind,
            "http_response"
        );
    }
    #[tokio::test]
    async fn disconnect_completes_inflight_request_as_error() {
        let relay = Arc::new(Relay::new());
        let id = "aistudio-worker-b";
        let (tx, mut rx) = mpsc::channel(8);
        let generation = relay.registry.insert(id.into(), tx);
        let task = {
            let r = relay.clone();
            tokio::spawn(async move {
                r.execute(
                    id,
                    "POST",
                    "https://fixture.invalid",
                    HeaderMap::new(),
                    vec![],
                )
                .await
            })
        };
        let _request = rx.recv().await.unwrap();
        relay.disconnect(id, generation);
        assert!(matches!(
            timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap(),
            Err(WorkerError::Disconnected)
        ));
    }
    #[tokio::test]
    async fn catalog_is_worker_owned_and_unknown_until_sent() {
        let relay = Relay::new();
        let (tx, _rx) = mpsc::channel(1);
        let generation = relay.registry.insert("aistudio-worker-c".into(), tx);
        assert_eq!(relay.registry.catalog("aistudio-worker-c"), None);
        relay.dispatch(
            "aistudio-worker-c",
            generation,
            Message {
                id: "catalog-1".into(),
                kind: "catalog".into(),
                payload: Some(json!({"models":["remote-model"]})),
            },
        );
        assert_eq!(
            relay.registry.catalog("aistudio-worker-c"),
            Some(vec!["remote-model".into()])
        );
    }
    #[tokio::test]
    async fn unmatched_response_does_not_complete_other_request() {
        let relay = Arc::new(Relay::new());
        let id = "aistudio-worker-d";
        let (tx, mut rx) = mpsc::channel(8);
        let generation = relay.registry.insert(id.into(), tx);
        let task = {
            let r = relay.clone();
            tokio::spawn(async move {
                r.execute(
                    id,
                    "POST",
                    "https://fixture.invalid",
                    HeaderMap::new(),
                    vec![],
                )
                .await
            })
        };
        let request = rx.recv().await.unwrap();
        relay.dispatch(
            id,
            generation,
            Message {
                id: "unmatched".into(),
                kind: "http_response".into(),
                payload: Some(json!({"status":200})),
            },
        );
        relay.disconnect(id, generation);
        assert!(matches!(
            timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap(),
            Err(WorkerError::Disconnected)
        ));
        let _ = request;
    }
}

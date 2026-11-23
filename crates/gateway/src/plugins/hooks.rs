use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

use crate::plugins::abi::*;
use crate::plugins::loader::{LoadedPlugin, PluginHost};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestInterceptRequest {
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub trace_id: String,
    #[serde(default)]
    pub source_format: String,
    #[serde(default)]
    pub to_format: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub requested_model: String,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub headers: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub body: Vec<u8>,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestInterceptResponse {
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub headers: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub body: Vec<u8>,
    #[serde(default)]
    pub clear_headers: Vec<String>,
    #[serde(default)]
    pub terminate: bool,
    #[serde(default)]
    pub status_code: u16,
    #[serde(default)]
    pub response_headers: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub response_body: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseInterceptRequest {
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub source_format: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub requested_model: String,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub request_headers: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub response_headers: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub original_request: Vec<u8>,
    #[serde(default)]
    pub request_body: Vec<u8>,
    #[serde(default)]
    pub body: Vec<u8>,
    #[serde(default)]
    pub status_code: u16,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResponseInterceptResponse {
    #[serde(default)]
    pub headers: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub body: Vec<u8>,
    #[serde(default)]
    pub clear_headers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamChunkInterceptRequest {
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub source_format: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub requested_model: String,
    #[serde(default)]
    pub request_headers: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub response_headers: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub original_request: Vec<u8>,
    #[serde(default)]
    pub request_body: Vec<u8>,
    #[serde(default)]
    pub body: Vec<u8>,
    #[serde(default)]
    pub history_chunks: Vec<Vec<u8>>,
    #[serde(default)]
    pub chunk_index: i64,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StreamChunkInterceptResponse {
    #[serde(default)]
    pub headers: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub body: Vec<u8>,
    #[serde(default)]
    pub clear_headers: Vec<String>,
    #[serde(default)]
    pub drop_chunk: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestCompletion {
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub trace_id: String,
    #[serde(default)]
    pub source_format: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub requested_model: String,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub outcome: String,
    #[serde(default)]
    pub status_code: u16,
    #[serde(default)]
    pub error: String,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WebSocketResponseEvent {
    #[serde(default)]
    pub request_id: String,
    #[serde(default)]
    pub trace_id: String,
    #[serde(default)]
    pub source_format: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub requested_model: String,
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub auth_id: String,
    #[serde(default)]
    pub auth_label: String,
    #[serde(default)]
    pub auth_type: String,
    #[serde(default)]
    pub event_type: String,
    #[serde(default)]
    pub payload: Vec<u8>,
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

pub struct PluginHookRegistry {
    host: Arc<PluginHost>,
}

impl PluginHookRegistry {
    pub fn new(host: Arc<PluginHost>) -> Self {
        Self { host }
    }

    pub fn intercept_request_before_auth(
        &self,
        mut req: RequestInterceptRequest,
    ) -> RequestInterceptResponse {
        let mut final_resp = RequestInterceptResponse {
            headers: req.headers.clone(),
            body: req.body.clone(),
            ..Default::default()
        };

        let plugins = self.host.list_plugins();
        for p in plugins {
            if !has_capability(&p, "request_interceptor") {
                continue;
            }
            let req_json = serde_json::to_vec(&req).unwrap_or_default();
            if let Ok(bytes) = p.client.call(METHOD_REQUEST_INTERCEPT_BEFORE, &req_json) {
                if let Ok(env) = serde_json::from_slice::<Envelope>(&bytes) {
                    if env.ok {
                        if let Some(res) = env.result {
                            if let Ok(item) =
                                serde_json::from_value::<RequestInterceptResponse>(res)
                            {
                                merge_request_intercept(&mut final_resp, &item);
                                if final_resp.terminate {
                                    break;
                                }
                                req.headers = final_resp.headers.clone();
                                if !final_resp.body.is_empty() {
                                    req.body = final_resp.body.clone();
                                }
                                if !final_resp.path.is_empty() {
                                    req.metadata
                                        .insert("path".to_string(), json!(final_resp.path));
                                }
                            }
                        }
                    }
                }
            }
        }
        final_resp
    }

    pub fn intercept_request_after_auth(
        &self,
        mut req: RequestInterceptRequest,
    ) -> RequestInterceptResponse {
        let mut final_resp = RequestInterceptResponse {
            headers: req.headers.clone(),
            body: req.body.clone(),
            ..Default::default()
        };

        let plugins = self.host.list_plugins();
        for p in plugins {
            if !has_capability(&p, "request_interceptor") {
                continue;
            }
            let req_json = serde_json::to_vec(&req).unwrap_or_default();
            if let Ok(bytes) = p.client.call(METHOD_REQUEST_INTERCEPT_AFTER, &req_json) {
                if let Ok(env) = serde_json::from_slice::<Envelope>(&bytes) {
                    if env.ok {
                        if let Some(res) = env.result {
                            if let Ok(item) =
                                serde_json::from_value::<RequestInterceptResponse>(res)
                            {
                                merge_request_intercept(&mut final_resp, &item);
                                if final_resp.terminate {
                                    break;
                                }
                                req.headers = final_resp.headers.clone();
                                if !final_resp.body.is_empty() {
                                    req.body = final_resp.body.clone();
                                }
                                if !final_resp.path.is_empty() {
                                    req.metadata
                                        .insert("path".to_string(), json!(final_resp.path));
                                }
                            }
                        }
                    }
                }
            }
        }
        final_resp
    }

    pub fn normalize_request(
        &self,
        from: &str,
        to: &str,
        model: &str,
        body: &[u8],
        stream: bool,
    ) -> Vec<u8> {
        let mut current = body.to_vec();
        let plugins = self.host.list_plugins();
        for p in plugins {
            if !has_capability(&p, "request_normalizer") {
                continue;
            }
            let payload = json!({
                "FromFormat": from,
                "ToFormat": to,
                "Model": model,
                "Stream": stream,
                "Body": current,
            });
            if let Ok(bytes) = p.client.call(
                METHOD_REQUEST_NORMALIZE,
                &serde_json::to_vec(&payload).unwrap_or_default(),
            ) {
                if let Ok(env) = serde_json::from_slice::<Envelope>(&bytes) {
                    if env.ok {
                        if let Some(res) = env.result {
                            if let Some(b) = res.get("Body").and_then(|v| v.as_str()) {
                                if let Ok(decoded) = base64_decode(b) {
                                    current = decoded;
                                }
                            }
                        }
                    }
                }
            }
        }
        current
    }

    pub fn translate_request(
        &self,
        from: &str,
        to: &str,
        model: &str,
        body: &[u8],
        stream: bool,
    ) -> (Vec<u8>, bool) {
        let plugins = self.host.list_plugins();
        let mut payload_bytes = None;
        for p in plugins {
            if !has_capability(&p, "request_translator") {
                continue;
            }
            let payload_bytes = payload_bytes.get_or_insert_with(|| {
                let payload = json!({
                    "FromFormat": from,
                    "ToFormat": to,
                    "Model": model,
                    "Stream": stream,
                    "Body": body,
                });
                serde_json::to_vec(&payload).unwrap_or_default()
            });
            if let Ok(bytes) = p.client.call(METHOD_REQUEST_TRANSLATE, payload_bytes) {
                if let Ok(env) = serde_json::from_slice::<Envelope>(&bytes) {
                    if env.ok {
                        if let Some(res) = env.result {
                            if let Some(b) = res.get("Body").and_then(|v| v.as_str()) {
                                if let Ok(decoded) = base64_decode(b) {
                                    return (decoded, true);
                                }
                            }
                        }
                    }
                }
            }
        }
        (body.to_vec(), false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn normalize_response_before(
        &self,
        from: &str,
        to: &str,
        model: &str,
        original_req: &[u8],
        request_raw: &[u8],
        body: &[u8],
        stream: bool,
    ) -> Vec<u8> {
        let mut current = body.to_vec();
        let plugins = self.host.list_plugins();
        for p in plugins {
            if !has_capability(&p, "response_before_translator") {
                continue;
            }
            let payload = json!({
                "FromFormat": from,
                "ToFormat": to,
                "Model": model,
                "Stream": stream,
                "OriginalRequest": original_req,
                "TranslatedRequest": request_raw,
                "Body": current,
            });
            if let Ok(bytes) = p.client.call(
                METHOD_RESPONSE_NORMALIZE_BEFORE,
                &serde_json::to_vec(&payload).unwrap_or_default(),
            ) {
                if let Ok(env) = serde_json::from_slice::<Envelope>(&bytes) {
                    if env.ok {
                        if let Some(res) = env.result {
                            if let Some(b) = res.get("Body").and_then(|v| v.as_str()) {
                                if let Ok(decoded) = base64_decode(b) {
                                    current = decoded;
                                }
                            }
                        }
                    }
                }
            }
        }
        current
    }

    #[allow(clippy::too_many_arguments)]
    pub fn translate_response(
        &self,
        from: &str,
        to: &str,
        model: &str,
        original_req: &[u8],
        request_raw: &[u8],
        body: &[u8],
        stream: bool,
    ) -> (Vec<u8>, bool) {
        let plugins = self.host.list_plugins();
        let mut payload_bytes = None;
        for p in plugins {
            if !has_capability(&p, "response_translator") {
                continue;
            }
            let payload_bytes = payload_bytes.get_or_insert_with(|| {
                let payload = json!({
                    "FromFormat": from,
                    "ToFormat": to,
                    "Model": model,
                    "Stream": stream,
                    "OriginalRequest": original_req,
                    "TranslatedRequest": request_raw,
                    "Body": body,
                });
                serde_json::to_vec(&payload).unwrap_or_default()
            });
            if let Ok(bytes) = p.client.call(METHOD_RESPONSE_TRANSLATE, payload_bytes) {
                if let Ok(env) = serde_json::from_slice::<Envelope>(&bytes) {
                    if env.ok {
                        if let Some(res) = env.result {
                            if let Some(b) = res.get("Body").and_then(|v| v.as_str()) {
                                if let Ok(decoded) = base64_decode(b) {
                                    return (decoded, true);
                                }
                            }
                        }
                    }
                }
            }
        }
        (body.to_vec(), false)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn normalize_response_after(
        &self,
        from: &str,
        to: &str,
        model: &str,
        original_req: &[u8],
        request_raw: &[u8],
        body: &[u8],
        stream: bool,
    ) -> Vec<u8> {
        let mut current = body.to_vec();
        let plugins = self.host.list_plugins();
        for p in plugins {
            if !has_capability(&p, "response_after_translator") {
                continue;
            }
            let payload = json!({
                "FromFormat": from,
                "ToFormat": to,
                "Model": model,
                "Stream": stream,
                "OriginalRequest": original_req,
                "TranslatedRequest": request_raw,
                "Body": current,
            });
            if let Ok(bytes) = p.client.call(
                METHOD_RESPONSE_NORMALIZE_AFTER,
                &serde_json::to_vec(&payload).unwrap_or_default(),
            ) {
                if let Ok(env) = serde_json::from_slice::<Envelope>(&bytes) {
                    if env.ok {
                        if let Some(res) = env.result {
                            if let Some(b) = res.get("Body").and_then(|v| v.as_str()) {
                                if let Ok(decoded) = base64_decode(b) {
                                    current = decoded;
                                }
                            }
                        }
                    }
                }
            }
        }
        current
    }

    pub fn intercept_response(&self, req: ResponseInterceptRequest) -> ResponseInterceptResponse {
        let mut final_resp = ResponseInterceptResponse {
            headers: req.response_headers.clone(),
            body: req.body.clone(),
            ..Default::default()
        };

        let plugins = self.host.list_plugins();
        let mut req_json = None;
        for p in plugins {
            if !has_capability(&p, "response_interceptor") {
                continue;
            }
            let req_json =
                req_json.get_or_insert_with(|| serde_json::to_vec(&req).unwrap_or_default());
            if let Ok(bytes) = p.client.call(METHOD_RESPONSE_INTERCEPT_AFTER, req_json) {
                if let Ok(env) = serde_json::from_slice::<Envelope>(&bytes) {
                    if env.ok {
                        if let Some(res) = env.result {
                            if let Ok(item) =
                                serde_json::from_value::<ResponseInterceptResponse>(res)
                            {
                                for key in &item.clear_headers {
                                    final_resp.headers.remove(key);
                                }
                                for (k, v) in item.headers {
                                    final_resp.headers.insert(k, v);
                                }
                                if !item.body.is_empty() {
                                    final_resp.body = item.body;
                                }
                            }
                        }
                    }
                }
            }
        }
        final_resp
    }

    pub fn intercept_stream_chunk(
        &self,
        req: StreamChunkInterceptRequest,
    ) -> StreamChunkInterceptResponse {
        let plugins = self.host.list_plugins();
        self.intercept_stream_chunk_for_plugins(req, plugins)
    }

    fn intercept_stream_chunk_for_plugins(
        &self,
        req: StreamChunkInterceptRequest,
        plugins: Vec<std::sync::Arc<LoadedPlugin>>,
    ) -> StreamChunkInterceptResponse {
        let mut final_resp = StreamChunkInterceptResponse {
            headers: req.response_headers.clone(),
            body: req.body.clone(),
            ..Default::default()
        };
        for p in plugins {
            if !has_capability(&p, "stream_chunk_interceptor") {
                continue;
            }
            #[derive(Serialize)]
            struct StreamChunkView<'a> {
                request_id: &'a str,
                source_format: &'a str,
                model: &'a str,
                requested_model: &'a str,
                request_headers: &'a HashMap<String, Vec<String>>,
                response_headers: &'a HashMap<String, Vec<String>>,
                original_request: &'a [u8],
                request_body: &'a [u8],
                body: &'a [u8],
                history_chunks: &'a [Vec<u8>],
                chunk_index: i64,
                metadata: &'a HashMap<String, serde_json::Value>,
            }
            let omit_request = req.chunk_index >= 0
                && p.schema_version >= SCHEMA_VERSION_STREAM_CHUNK_OMIT_REQUEST_BODY;
            let omit_history = req.chunk_index >= 0
                && p.schema_version >= SCHEMA_VERSION_STREAM_CHUNK_OMIT_HISTORY;
            let view = StreamChunkView {
                request_id: &req.request_id,
                source_format: &req.source_format,
                model: &req.model,
                requested_model: &req.requested_model,
                request_headers: &req.request_headers,
                response_headers: &req.response_headers,
                original_request: if omit_request {
                    &[]
                } else {
                    &req.original_request
                },
                request_body: if omit_request { &[] } else { &req.request_body },
                body: &req.body,
                history_chunks: if omit_history {
                    &[]
                } else {
                    &req.history_chunks
                },
                chunk_index: req.chunk_index,
                metadata: &req.metadata,
            };
            let req_json = serde_json::to_vec(&view).unwrap_or_default();
            if let Ok(bytes) = p
                .client
                .call(METHOD_RESPONSE_INTERCEPT_STREAM_CHUNK, &req_json)
            {
                if let Ok(env) = serde_json::from_slice::<Envelope>(&bytes) {
                    if env.ok {
                        if let Some(res) = env.result {
                            if let Ok(item) =
                                serde_json::from_value::<StreamChunkInterceptResponse>(res)
                            {
                                for key in &item.clear_headers {
                                    final_resp.headers.remove(key);
                                }
                                for (k, v) in item.headers {
                                    final_resp.headers.insert(k, v);
                                }
                                if !item.body.is_empty() {
                                    final_resp.body = item.body;
                                }
                                if item.drop_chunk {
                                    final_resp.drop_chunk = true;
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
        final_resp
    }

    pub fn complete_request(&self, completion: RequestCompletion) {
        let plugins = self.host.list_plugins();
        let mut req_json = None;
        for p in plugins {
            if !has_capability(&p, "request_lifecycle_plugin") {
                continue;
            }
            let req_json =
                req_json.get_or_insert_with(|| serde_json::to_vec(&completion).unwrap_or_default());
            let _ = p.client.call(METHOD_REQUEST_COMPLETE, req_json);
        }
    }

    pub fn observe_websocket_response_event(&self, event: WebSocketResponseEvent) {
        let plugins = self.host.list_plugins();
        let mut req_json = None;
        for p in plugins {
            if p.schema_version < SCHEMA_VERSION_WEBSOCKET_RESPONSE_OBSERVER {
                continue;
            }
            if !has_capability(&p, "websocket_response_observer") {
                continue;
            }
            let req_json =
                req_json.get_or_insert_with(|| serde_json::to_vec(&event).unwrap_or_default());
            let _ = p.client.call(METHOD_WEBSOCKET_RESPONSE_EVENT, req_json);
        }
    }
}

fn has_capability(plugin: &LoadedPlugin, key: &str) -> bool {
    plugin
        .capabilities
        .get(key)
        .map(|v| match v {
            serde_json::Value::Bool(b) => *b,
            serde_json::Value::Null => false,
            _ => true,
        })
        .unwrap_or(false)
}

fn merge_request_intercept(target: &mut RequestInterceptResponse, next: &RequestInterceptResponse) {
    if !next.path.is_empty() {
        target.path = next.path.clone();
    }
    for key in &next.clear_headers {
        target.headers.remove(key);
    }
    for (k, v) in &next.headers {
        target.headers.insert(k.clone(), v.clone());
    }
    if !next.body.is_empty() {
        target.body = next.body.clone();
    }
    if next.terminate {
        target.terminate = true;
        target.status_code = next.status_code;
        target.response_headers = next.response_headers.clone();
        target.response_body = next.response_body.clone();
    }
}

fn base64_decode(s: &str) -> Result<Vec<u8>, ()> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|_| ())
}

#[cfg(test)]
mod review_plugins_tests {
    use super::*;
    use crate::plugins::loader::PluginClient;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Arc, Mutex};

    struct Recorder(u32, Mutex<Vec<Vec<u8>>>);
    impl PluginClient for Recorder {
        fn call(&self, method: &str, request: &[u8]) -> Result<Vec<u8>, PluginError> {
            let value = if method == METHOD_PLUGIN_REGISTER {
                json!({"schema_version": self.0, "capabilities": {"stream_chunk_interceptor": true}})
            } else {
                self.1.lock().unwrap().push(request.to_vec());
                json!({"body": [7]})
            };
            Ok(serde_json::to_vec(&Envelope::success(value)).unwrap())
        }
        fn shutdown(&self) {}
    }

    struct FanoutRecorder(Mutex<HashMap<String, Vec<Vec<u8>>>>);

    impl PluginClient for FanoutRecorder {
        fn call(&self, method: &str, request: &[u8]) -> Result<Vec<u8>, PluginError> {
            let value = if method == METHOD_PLUGIN_REGISTER {
                json!({"schema_version":6,"capabilities":{
                    "request_translator":true,"response_translator":true,
                    "response_interceptor":true,"request_lifecycle_plugin":true,
                    "websocket_response_observer":true
                }})
            } else {
                self.0
                    .lock()
                    .unwrap()
                    .entry(method.into())
                    .or_default()
                    .push(request.to_vec());
                json!({"body":[7]})
            };
            Ok(serde_json::to_vec(&Envelope::success(value)).unwrap())
        }
        fn shutdown(&self) {}
    }

    #[test]
    fn ulw_gw5_immutable_fanouts_keep_identical_wire_input() {
        let host = Arc::new(PluginHost::new(Arc::new(|_, _, _| Ok(vec![]))));
        let clients: Vec<_> = (0..3)
            .map(|index| {
                let client = Arc::new(FanoutRecorder(Mutex::new(HashMap::new())));
                host.register_plugin_client(
                    &index.to_string(),
                    std::path::Path::new("fixture"),
                    "1",
                    client.clone(),
                    Arc::new(AtomicUsize::new(0)),
                )
                .unwrap();
                client
            })
            .collect();
        let hooks = PluginHookRegistry::new(host);
        assert_eq!(
            hooks.translate_request("a", "b", "m", b"body", true),
            (b"body".to_vec(), false)
        );
        assert_eq!(
            hooks.translate_response("a", "b", "m", b"original", b"request", b"body", true),
            (b"body".to_vec(), false)
        );
        let response: ResponseInterceptRequest = serde_json::from_value(json!({
            "body":[1,2],"response_headers":{"x-fixture":["value"]},"metadata":{"fixture":true}
        }))
        .unwrap();
        let response_bytes = serde_json::to_vec(&response).unwrap();
        assert_eq!(hooks.intercept_response(response).body, [7]);
        let completion: RequestCompletion =
            serde_json::from_value(json!({"request_id":"fixture"})).unwrap();
        let completion_bytes = serde_json::to_vec(&completion).unwrap();
        hooks.complete_request(completion);
        let event: WebSocketResponseEvent =
            serde_json::from_value(json!({"payload":[0,255]})).unwrap();
        let event_bytes = serde_json::to_vec(&event).unwrap();
        hooks.observe_websocket_response_event(event);
        let expected = [
            (METHOD_REQUEST_TRANSLATE, serde_json::to_vec(&json!({"FromFormat":"a","ToFormat":"b","Model":"m","Stream":true,"Body":b"body"})).unwrap()),
            (METHOD_RESPONSE_TRANSLATE, serde_json::to_vec(&json!({"FromFormat":"a","ToFormat":"b","Model":"m","Stream":true,"OriginalRequest":b"original","TranslatedRequest":b"request","Body":b"body"})).unwrap()),
            (METHOD_RESPONSE_INTERCEPT_AFTER, response_bytes),
            (METHOD_REQUEST_COMPLETE, completion_bytes),
            (METHOD_WEBSOCKET_RESPONSE_EVENT, event_bytes),
        ];
        for client in clients {
            let calls = client.0.lock().unwrap();
            assert_eq!(calls.len(), expected.len());
            for (method, bytes) in &expected {
                assert_eq!(calls[*method], vec![bytes.clone()]);
            }
        }
    }

    #[test]
    fn review_plugins_mixed_schema_stream_views_are_independent() {
        let host = Arc::new(PluginHost::new(Arc::new(|_, _, _| Ok(vec![]))));
        let clients: Vec<_> = [6, 2, 4]
            .into_iter()
            .map(|schema| Arc::new(Recorder(schema, Mutex::new(vec![]))))
            .collect();
        let plugins = clients
            .iter()
            .map(|client| {
                host.register_plugin_client(
                    &format!("schema-{}", client.0),
                    std::path::Path::new("fixture"),
                    "1",
                    client.clone(),
                    Arc::new(AtomicUsize::new(0)),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        let hooks = PluginHookRegistry::new(host);
        for chunk_index in [0, -1] {
            let request: StreamChunkInterceptRequest = serde_json::from_value(json!({
                "original_request": [1], "request_body": [2], "history_chunks": [[3]], "body": [4], "chunk_index": chunk_index
            })).unwrap();
            let original = request.clone();
            assert_eq!(
                hooks
                    .intercept_stream_chunk_for_plugins(request, plugins.clone())
                    .body,
                vec![7]
            );
            for client in &clients {
                let mut calls = client.1.lock().unwrap();
                let received_bytes = calls.pop().unwrap();
                let mut expected = original.clone();
                if chunk_index >= 0 && client.0 >= 3 {
                    expected.original_request.clear();
                    expected.request_body.clear();
                }
                if chunk_index >= 0 && client.0 >= 5 {
                    expected.history_chunks.clear();
                }
                assert_eq!(received_bytes, serde_json::to_vec(&expected).unwrap());
                let received: StreamChunkInterceptRequest =
                    serde_json::from_slice(&received_bytes).unwrap();
                assert_eq!(
                    received.original_request,
                    if chunk_index >= 0 && client.0 >= 3 {
                        vec![]
                    } else {
                        vec![1]
                    }
                );
                assert_eq!(
                    received.request_body,
                    if chunk_index >= 0 && client.0 >= 3 {
                        vec![]
                    } else {
                        vec![2]
                    }
                );
                assert_eq!(
                    received.history_chunks,
                    if chunk_index >= 0 && client.0 >= 5 {
                        vec![]
                    } else {
                        vec![vec![3]]
                    }
                );
                assert_eq!(received.body, vec![4]);
            }
        }
    }
}

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
        for p in plugins {
            if !has_capability(&p, "request_translator") {
                continue;
            }
            let payload = json!({
                "FromFormat": from,
                "ToFormat": to,
                "Model": model,
                "Stream": stream,
                "Body": body,
            });
            if let Ok(bytes) = p.client.call(
                METHOD_REQUEST_TRANSLATE,
                &serde_json::to_vec(&payload).unwrap_or_default(),
            ) {
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
        for p in plugins {
            if !has_capability(&p, "response_translator") {
                continue;
            }
            let payload = json!({
                "FromFormat": from,
                "ToFormat": to,
                "Model": model,
                "Stream": stream,
                "OriginalRequest": original_req,
                "TranslatedRequest": request_raw,
                "Body": body,
            });
            if let Ok(bytes) = p.client.call(
                METHOD_RESPONSE_TRANSLATE,
                &serde_json::to_vec(&payload).unwrap_or_default(),
            ) {
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
        for p in plugins {
            if !has_capability(&p, "response_interceptor") {
                continue;
            }
            let req_json = serde_json::to_vec(&req).unwrap_or_default();
            if let Ok(bytes) = p.client.call(METHOD_RESPONSE_INTERCEPT_AFTER, &req_json) {
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
        mut req: StreamChunkInterceptRequest,
    ) -> StreamChunkInterceptResponse {
        let mut final_resp = StreamChunkInterceptResponse {
            headers: req.response_headers.clone(),
            body: req.body.clone(),
            ..Default::default()
        };

        let plugins = self.host.list_plugins();
        for p in plugins {
            if !has_capability(&p, "stream_chunk_interceptor") {
                continue;
            }
            if req.chunk_index >= 0 {
                if p.schema_version >= SCHEMA_VERSION_STREAM_CHUNK_OMIT_REQUEST_BODY {
                    req.original_request = Vec::new();
                    req.request_body = Vec::new();
                }
                if p.schema_version >= SCHEMA_VERSION_STREAM_CHUNK_OMIT_HISTORY {
                    req.history_chunks = Vec::new();
                }
            }

            let req_json = serde_json::to_vec(&req).unwrap_or_default();
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
        for p in plugins {
            if !has_capability(&p, "request_lifecycle_plugin") {
                continue;
            }
            let req_json = serde_json::to_vec(&completion).unwrap_or_default();
            let _ = p.client.call(METHOD_REQUEST_COMPLETE, &req_json);
        }
    }

    pub fn observe_websocket_response_event(&self, event: WebSocketResponseEvent) {
        let plugins = self.host.list_plugins();
        for p in plugins {
            if p.schema_version < SCHEMA_VERSION_WEBSOCKET_RESPONSE_OBSERVER {
                continue;
            }
            if !has_capability(&p, "websocket_response_observer") {
                continue;
            }
            let req_json = serde_json::to_vec(&event).unwrap_or_default();
            let _ = p.client.call(METHOD_WEBSOCKET_RESPONSE_EVENT, &req_json);
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

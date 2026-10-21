//! Meta uses Responses SSE for both streaming and non-streaming calls.
use super::request::{openai_to_codex, TranslateError};
use serde_json::{json, Value};

pub fn prepare_request(raw: &[u8], model: &str) -> Result<Vec<u8>, TranslateError> {
    let mut body: Value =
        serde_json::from_slice(raw).map_err(|e| TranslateError::Json(e.to_string()))?;
    if body.get("messages").is_some() {
        body = serde_json::from_slice(&openai_to_codex(raw)?.body)
            .map_err(|e| TranslateError::Json(e.to_string()))?;
    }
    let object = body
        .as_object_mut()
        .ok_or(TranslateError::Missing("body"))?;
    if !object.contains_key("input") {
        return Err(TranslateError::Missing("input"));
    }
    object.insert("model".into(), json!(model));
    object.insert("stream".into(), json!(true));
    for key in [
        "generate",
        "prompt_cache_retention",
        "safety_identifier",
        "stream_options",
        "client_metadata",
    ] {
        object.remove(key);
    }
    match object.get("instructions") {
        None | Some(Value::Null) => {
            object.insert("instructions".into(), json!(""));
        }
        Some(Value::String(_)) => {}
        Some(_) => {
            return Err(TranslateError::UnsupportedOption(
                "instructions must be a string".into(),
            ))
        }
    }
    let store = object
        .get("store")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(input) = object.get_mut("input").and_then(Value::as_array_mut) {
        for item in input {
            if item.get("type").and_then(Value::as_str) != Some("reasoning") {
                continue;
            }
            let Some(reasoning) = item.as_object_mut() else {
                continue;
            };
            if let Some(content) = reasoning
                .get("content")
                .and_then(Value::as_array)
                .filter(|v| !v.is_empty())
            {
                let summary_empty = reasoning
                    .get("summary")
                    .is_none_or(|s| s.is_null() || s.as_array().is_some_and(Vec::is_empty));
                if summary_empty {
                    let summary: Vec<Value> = content
                        .iter()
                        .filter(|p| p.get("type").and_then(Value::as_str) == Some("reasoning_text"))
                        .filter_map(|p| p.get("text").and_then(Value::as_str))
                        .filter(|s| !s.is_empty())
                        .map(|text| json!({"type":"summary_text","text":text}))
                        .collect();
                    if !summary.is_empty() {
                        reasoning.insert("summary".into(), json!(summary));
                    }
                }
                reasoning.insert("content".into(), json!([]));
            }
            let usable = reasoning
                .get("encrypted_content")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.is_empty() && s == s.trim() && !known_foreign_signature(s));
            if !usable {
                reasoning.remove("encrypted_content");
                if !store {
                    reasoning.remove("id");
                }
            }
        }
    }
    if let Some(tools) = object.get_mut("tools").and_then(Value::as_array_mut) {
        sanitize_tools(tools);
    }
    serde_json::to_vec(&body).map_err(|e| TranslateError::Json(e.to_string()))
}

fn sanitize_tools(tools: &mut [Value]) {
    for tool in tools {
        match tool.get("type").and_then(Value::as_str) {
            Some("web_search") => {
                if let Some(object) = tool.as_object_mut() {
                    object.remove("search_content_types");
                }
            }
            Some("namespace") => {
                if let Some(nested) = tool.get_mut("tools").and_then(Value::as_array_mut) {
                    sanitize_tools(nested);
                }
            }
            _ => {}
        }
    }
}

fn known_foreign_signature(raw: &str) -> bool {
    use base64::Engine;
    let signature = raw.split_once('#').map_or(raw, |(_, value)| value);
    if signature.starts_with("gAAAA") {
        return false;
    }
    if signature.starts_with("sealed.v1.")
        || matches!(
            signature,
            "skip_thought_signature_validator" | "context_engineering_is_the_way_to_go"
        )
    {
        return true;
    }
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(signature)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(signature));
    let Ok(mut bytes) = decoded else {
        return false;
    };
    if let Ok(inner) = base64::engine::general_purpose::STANDARD.decode(&bytes) {
        bytes = inner;
    }
    if let Some(container) = signature_bytes_field(&bytes, 2) {
        if let Some(channel) = signature_bytes_field(container, 1) {
            if channel.first() == Some(&8) {
                return true;
            }
            if channel.first() == Some(&1) {
                return true;
            }
        }
        if let Some(channel) = signature_bytes_field(container, 3) {
            if channel.first() == Some(&8) {
                return true;
            }
        }
    }
    if matches!(signature.len(), 4340 | 12946) && !signature.contains('=') {
        let mut counts = [0_u32; 256];
        for byte in &bytes {
            counts[usize::from(*byte)] += 1;
        }
        let length = bytes.len() as f64;
        let entropy: f64 = counts
            .iter()
            .filter(|count| **count > 0)
            .map(|count| {
                let probability = f64::from(*count) / length;
                -probability * probability.log2()
            })
            .sum();
        return entropy / length.min(256.0).log2() >= 0.85;
    }
    false
}

fn signature_bytes_field(mut bytes: &[u8], wanted: u64) -> Option<&[u8]> {
    use bytes::Buf;
    while bytes.has_remaining() {
        let tag = prost::encoding::decode_varint(&mut bytes).ok()?;
        let field = tag >> 3;
        match tag & 7 {
            0 => {
                prost::encoding::decode_varint(&mut bytes).ok()?;
            }
            1 => {
                if bytes.len() < 8 {
                    return None;
                }
                bytes = &bytes[8..];
            }
            2 => {
                let length =
                    usize::try_from(prost::encoding::decode_varint(&mut bytes).ok()?).ok()?;
                if length > bytes.len() {
                    return None;
                }
                let (value, rest) = bytes.split_at(length);
                bytes = rest;
                if field == wanted {
                    return Some(value);
                }
            }
            5 => {
                if bytes.len() < 4 {
                    return None;
                }
                bytes = &bytes[4..];
            }
            _ => return None,
        }
    }
    None
}

/// Preserve complete output items (including opaque encrypted reasoning) omitted
/// from the terminal event. No reconstruction or account-global replay cache.
#[derive(Default)]
pub struct MetaOutput {
    indexed: std::collections::BTreeMap<u64, Value>,
    fallback: Vec<Value>,
    completed: bool,
}

impl MetaOutput {
    pub fn observe(&mut self, event: &mut Value) -> Result<(), String> {
        match event.get("type").and_then(Value::as_str) {
            Some("error" | "response.failed") => return Err("Meta upstream response failed".into()),
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    if let Some(index) = event.get("output_index").and_then(Value::as_u64) {
                        self.indexed.insert(index, item.clone());
                    } else {
                        self.fallback.push(item.clone());
                    }
                }
            }
            Some("response.completed" | "response.incomplete") => {
                let response = event
                    .get_mut("response")
                    .and_then(Value::as_object_mut)
                    .ok_or("Meta terminal event missing response")?;
                if response
                    .get("output")
                    .is_none_or(|v| v.is_null() || v.as_array().is_some_and(Vec::is_empty))
                {
                    let output: Vec<Value> = self
                        .indexed
                        .values()
                        .chain(self.fallback.iter())
                        .cloned()
                        .collect();
                    response.insert("output".into(), json!(output));
                }
                self.completed = true;
            }
            _ => {}
        }
        Ok(())
    }

    pub fn finish(&self) -> Result<(), String> {
        if self.completed {
            Ok(())
        } else {
            Err("Meta stream disconnected before terminal response".into())
        }
    }
}

pub async fn refresh_member(
    member: &crate::account::AccountMember,
    client: &reqwest::Client,
    presented_token: Option<&str>,
) -> Result<bool, String> {
    use mahoquot_providers::meta::MetaAccount;
    // The caller (`AccountMember::refresh`) already holds `refresh_lock`;
    // tokio's Mutex is not re-entrant, so taking it again here deadlocked the
    // reactive 401 re-mint.
    if presented_token.is_some_and(|token| member.access_token() != token) {
        return Ok(false);
    }
    let bytes = tokio::fs::read(&member.file_path)
        .await
        .map_err(|e| e.to_string())?;
    let account: MetaAccount = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if presented_token.is_none() && !account.needs_mint() {
        return Ok(false);
    }
    if account.dca().is_none() {
        return Ok(false);
    }
    let endpoints = endpoints_from_account(&account, member.upstream_override.clone());
    let candidate = mahoquot_providers::meta::refresh(
        client,
        &endpoints,
        &account,
        chrono::Utc::now().timestamp(),
    )
    .await
    .map_err(|e| e.to_string())?;
    persist_account(member.file_path.clone(), &candidate).await?;
    member.reload_from_file().map_err(|e| e.to_string())?;
    Ok(true)
}

pub fn endpoints_from_account(
    account: &mahoquot_providers::meta::MetaAccount,
    upstream_override: Option<String>,
) -> mahoquot_providers::meta::MetaEndpoints {
    let mut endpoints = mahoquot_providers::meta::MetaEndpoints {
        upstream_override,
        ..Default::default()
    };
    if let Some(mint) = account.metadata.get("mint_url").and_then(Value::as_str) {
        endpoints.mint = mint.to_string();
    }
    endpoints
}

async fn persist_account(
    path: std::path::PathBuf,
    account: &mahoquot_providers::meta::MetaAccount,
) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(account).map_err(|e| e.to_string())?;
    tokio::task::spawn_blocking(move || {
        mahoquot_providers::credential_file::write_credential_atomically(&path, &bytes)
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

pub async fn onboard_device(
    state: &crate::state::AppState,
    endpoints: &mahoquot_providers::meta::MetaEndpoints,
    token: mahoquot_providers::meta::DeviceToken,
) -> Result<String, String> {
    let client = state
        .try_client_for_target("meta", "device-onboarding")
        .map_err(|error| error.to_string())?;
    let mut account = mahoquot_providers::meta::complete_device(
        &client,
        endpoints,
        token,
        chrono::Utc::now().timestamp(),
    )
    .await
    .map_err(|e| e.to_string())?;
    let identity = if account.email.is_empty() {
        account.dca_token.as_str()
    } else {
        account.email.as_str()
    };
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(identity.trim().as_bytes());
    let suffix: String = digest
        .iter()
        .take(8)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    account.identity_slug = format!("meta-{suffix}");
    let dir = std::path::PathBuf::from(&state.settings.current().auth_dir);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;
    let path = dir.join(format!("{}.json", account.identity_slug));
    if let Ok(bytes) = tokio::fs::read(&path).await {
        let existing: mahoquot_providers::meta::MetaAccount =
            serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        account.identity_slug = existing.identity_slug;
        account.disabled = existing.disabled;
        for (key, value) in existing.metadata {
            if !matches!(
                key.as_str(),
                "subs_tier_name"
                    | "subs_tier_id"
                    | "is_subs_active"
                    | "has_payment_method"
                    | "require_payment"
                    | "can_subscribe"
            ) {
                account.metadata.entry(key).or_insert(value);
            }
        }
    }
    if let Some(base) = &endpoints.upstream_override {
        account
            .metadata
            .insert("upstream_override".into(), json!(base));
    }
    account
        .metadata
        .insert("mint_url".into(), json!(endpoints.mint));
    persist_account(path, &account).await?;
    state.rescan_pool().map_err(|e| e.to_string())?;
    Ok(account.identity_slug)
}

#[derive(serde::Deserialize, Default)]
#[serde(default)]
pub struct MetaDeviceParams {
    device_url: Option<String>,
    token_url: Option<String>,
    mint_url: Option<String>,
    upstream_override: Option<String>,
    device_code: String,
}

impl MetaDeviceParams {
    fn endpoints(&self) -> mahoquot_providers::meta::MetaEndpoints {
        let mut endpoints = mahoquot_providers::meta::MetaEndpoints::default();
        if let Some(url) = &self.device_url {
            endpoints.device = url.clone();
        }
        if let Some(url) = &self.token_url {
            endpoints.token = url.clone();
        }
        if let Some(url) = &self.mint_url {
            endpoints.mint = url.clone();
        }
        endpoints.upstream_override = self.upstream_override.clone();
        endpoints
    }
}

pub fn onboarding_routes() -> axum::Router<std::sync::Arc<crate::state::AppState>> {
    use axum::{
        extract::{Query, State},
        routing::{get, post},
        Json,
    };
    use mahoquot_providers::meta::{DeviceCode, PollResult};
    axum::Router::new()
        .route("/meta-auth-url", get(|State(state): State<std::sync::Arc<crate::state::AppState>>, Query(params): Query<MetaDeviceParams>| async move {
            let client = match state.try_client_for_target("meta", "device-onboarding") {
                Ok(client) => client,
                Err(error) => return (axum::http::StatusCode::BAD_REQUEST, Json(json!({"status":"error","error":error.to_string()}))),
            };
            match mahoquot_providers::meta::start_device(&client, &params.endpoints()).await {
                Ok(device) => (axum::http::StatusCode::OK, Json(json!({"provider":"meta","flow":"device","device_code":device.device_code,"user_code":device.user_code,"url":if device.verification_uri_complete.is_empty() { device.verification_uri } else { device.verification_uri_complete },"expires_in":device.expires_in,"interval":device.interval}))),
                Err(error) => (axum::http::StatusCode::BAD_REQUEST, Json(json!({"status":"error","error":error.to_string()}))),
            }
        }))
        .route("/meta-auth-complete", post(|State(state): State<std::sync::Arc<crate::state::AppState>>, Json(params): Json<MetaDeviceParams>| async move {
            let endpoints = params.endpoints();
            let device = DeviceCode { device_code: params.device_code, user_code: String::new(), verification_uri: String::new(), verification_uri_complete: String::new(), expires_in: 0, interval: 0 };
            let client = match state.try_client_for_target("meta", "device-onboarding") {
                Ok(client) => client,
                Err(error) => return (axum::http::StatusCode::BAD_REQUEST, Json(json!({"status":"error","error":error.to_string()}))),
            };
            let result = match mahoquot_providers::meta::poll_device(&client, &endpoints, &device).await {
                Ok(PollResult::Authorized(token)) => onboard_device(&state, &endpoints, token).await.map(|id| json!({"status":"completed","account_id":id})),
                Ok(PollResult::Pending) => Ok(json!({"status":"pending"})),
                Ok(PollResult::SlowDown) => Ok(json!({"status":"pending","interval_increase":5})),
                Err(error) => Err(error.to_string()),
            };
            match result {
                Ok(body) => (axum::http::StatusCode::OK, Json(body)),
                Err(error) => (axum::http::StatusCode::BAD_REQUEST, Json(json!({"status":"error","error":error}))),
            }
        }))
}

pub async fn completed_response(
    first: bytes::Bytes,
    stream: super::UpstreamStream,
) -> Result<Value, String> {
    let bytes = super::collect_stream(first, stream).await?;
    let mut parser = super::events::SseParser::default();
    let mut frames = Vec::new();
    parser
        .push_raw_data(&bytes, &mut frames)
        .map_err(|e| e.message().to_string())?;
    let mut output = MetaOutput::default();
    for frame in frames {
        let mut event: Value = serde_json::from_slice(&frame).map_err(|e| e.to_string())?;
        output.observe(&mut event)?;
        if matches!(
            event.get("type").and_then(Value::as_str),
            Some("response.completed" | "response.incomplete")
        ) {
            return event
                .get("response")
                .cloned()
                .ok_or("Meta terminal response missing".into());
        }
    }
    output.finish()?;
    Err("Meta terminal response missing".into())
}

pub fn responses_stream(first: bytes::Bytes, upstream: super::UpstreamStream) -> axum::body::Body {
    use futures::StreamExt;
    struct State {
        source: super::UpstreamStream,
        parser: super::events::SseParser,
        output: MetaOutput,
        pending: std::collections::VecDeque<bytes::Bytes>,
        ended: bool,
    }
    let source: super::UpstreamStream =
        Box::pin(futures::stream::once(async move { Ok(first) }).chain(upstream));
    let state = State {
        source,
        parser: Default::default(),
        output: Default::default(),
        pending: Default::default(),
        ended: false,
    };
    axum::body::Body::from_stream(futures::stream::unfold(state, |mut state| async move {
        loop {
            if let Some(frame) = state.pending.pop_front() {
                return Some((Ok::<_, std::io::Error>(frame), state));
            }
            if state.ended {
                return None;
            }
            match state.source.next().await {
                Some(Ok(chunk)) => {
                    let mut frames = Vec::new();
                    if let Err(error) = state.parser.push_raw_data(&chunk, &mut frames) {
                        state.ended = true;
                        return Some((Err(std::io::Error::other(error.message())), state));
                    }
                    for frame in frames {
                        if frame == b"[DONE]" {
                            continue;
                        }
                        let result = serde_json::from_slice::<Value>(&frame)
                            .map_err(|e| e.to_string())
                            .and_then(|mut event| {
                                state.output.observe(&mut event)?;
                                Ok(bytes::Bytes::from(format!("data: {event}\n\n")))
                            });
                        match result {
                            Ok(frame) => state.pending.push_back(frame),
                            Err(error) => {
                                state.ended = true;
                                return Some((Err(std::io::Error::other(error)), state));
                            }
                        }
                    }
                }
                Some(Err(error)) => {
                    state.ended = true;
                    return Some((Err(std::io::Error::other(error.to_string())), state));
                }
                None => {
                    state.ended = true;
                    if let Err(error) = state.output.finish() {
                        return Some((Err(std::io::Error::other(error)), state));
                    }
                }
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_foreign_envelopes_are_removed_without_changing_muse_ciphertext() {
        let body = json!({"input":[{"type":"reasoning","id":"foreign","encrypted_content":"sealed.v1.foreign"},
            {"type":"reasoning","id":"muse","encrypted_content":"muse-opaque+/="}]});
        let prepared: Value = serde_json::from_slice(
            &prepare_request(body.to_string().as_bytes(), "remote").unwrap(),
        )
        .unwrap();
        assert!(prepared["input"][0].get("encrypted_content").is_none());
        assert!(prepared["input"][0].get("id").is_none());
        assert_eq!(prepared["input"][1]["encrypted_content"], "muse-opaque+/=");
    }

    #[test]
    fn opaque_reasoning_round_trip_preserves_ciphertext_and_order() {
        let raw = br#"{"model":"m","input":[{"type":"reasoning","id":"r","encrypted_content":"muse-opaque+/=","content":[{"type":"reasoning_text","text":"thought"}]},{"role":"user","content":"next"}],"tools":[{"type":"web_search","search_content_types":["text"]},{"type":"web_search_preview","search_content_types":["text"]}]}"#;
        let result: Value =
            serde_json::from_slice(&prepare_request(raw, "remote").unwrap()).unwrap();
        assert_eq!(result["input"][0]["encrypted_content"], "muse-opaque+/=");
        assert_eq!(result["input"][0]["id"], "r");
        assert_eq!(result["input"][0]["content"], json!([]));
        assert_eq!(result["input"][0]["summary"][0]["text"], "thought");
        assert_eq!(result["input"][1]["role"], "user");
        assert!(result["tools"][0].get("search_content_types").is_none());
        assert_eq!(result["tools"][1]["search_content_types"], json!(["text"]));
        assert_eq!(result["stream"], true);
    }

    #[test]
    fn invalid_ciphertext_drops_orphan_id_unless_stored() {
        for encrypted in [json!(null), json!(false), json!(" padded "), json!("")] {
            let raw =
                json!({"input":[{"type":"reasoning","id":"orphan","encrypted_content":encrypted}]})
                    .to_string();
            let result: Value =
                serde_json::from_slice(&prepare_request(raw.as_bytes(), "m").unwrap()).unwrap();
            assert!(result["input"][0].get("id").is_none());
            assert!(result["input"][0].get("encrypted_content").is_none());
        }
        let result: Value = serde_json::from_slice(
            &prepare_request(
                br#"{"store":true,"input":[{"type":"reasoning","id":"stored"}]}"#,
                "m",
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(result["input"][0]["id"], "stored");
    }

    #[test]
    fn terminal_output_recovers_indexed_reasoning_without_cross_request_state() {
        let mut output = MetaOutput::default();
        output.observe(&mut json!({"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"second"}})).unwrap();
        output.observe(&mut json!({"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","encrypted_content":"opaque"}})).unwrap();
        let mut terminal = json!({"type":"response.completed","response":{"output":[]}});
        output.observe(&mut terminal).unwrap();
        assert_eq!(
            terminal["response"]["output"][0]["encrypted_content"],
            "opaque"
        );
        assert_eq!(terminal["response"]["output"][1]["id"], "second");
        assert!(output.finish().is_ok());
        assert!(MetaOutput::default().finish().is_err());
    }

    #[test]
    fn failed_event_never_becomes_successful_completion() {
        let mut output = MetaOutput::default();
        assert!(output
            .observe(&mut json!({"type":"response.failed","response":{"error":{"code":401}}}))
            .is_err());
        assert!(output.finish().is_err());
    }
}

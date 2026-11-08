//! Claude Code 2.1.316 wire profile for Claude OAuth accounts sent straight to
//! api.anthropic.com. Captures the Linux/x64 sdk-cli profile and allows optional
//! coherent native profile preservation.

use serde_json::{json, Map, Value};

pub const CLI_VERSION: &str = "2.1.316";
pub const USER_AGENT_SDK_CLI: &str = "claude-cli/2.1.316 (external, sdk-cli)";
pub const USER_AGENT_CLI: &str = "claude-cli/2.1.316 (external, cli)";
const FINGERPRINT_SALT: &str = "59cf53e54c78";
const CCH_SEED: u64 = 0x4D65_9218_E32A_3268;
const BILLING_PREFIX: &str = "x-anthropic-billing-header:";

const LEGACY_MODELS: &[&str] = &[
    "claude-3-5-haiku-20241022", "claude-3-5-haiku-latest", "claude-3-7-sonnet-20250219",
    "claude-3-7-sonnet-latest", "claude-haiku-4-5", "claude-haiku-4-5-20251001", "claude-opus-4",
    "claude-opus-4-20250514", "claude-opus-4-1", "claude-opus-4-1-20250805", "claude-opus-4-5",
    "claude-opus-4-5-20251101", "claude-opus-4-6", "claude-opus-4-7", "claude-sonnet-4",
    "claude-sonnet-4-20250514", "claude-sonnet-4-5", "claude-sonnet-4-5-20250929", "claude-sonnet-4-6",
];

/// Betas the profile computes itself; any other caller beta is newer than the
/// pinned profile and is forwarded verbatim (prompt-caching-2024-07-31 is an
/// Agent SDK-era beta the CLI no longer sends, so it is managed and dropped).
const MANAGED_BETAS: &[&str] = &[
    "token-counting-2024-11-01", "fast-mode-2026-02-01", "oauth-2025-04-20", "claude-code-20250219",
    "context-1m-2025-08-07", "mid-conversation-system-2026-04-07", "per-turn-control-2026-07-01",
    "timing-2026-09-09", "mid-conversation-tool-changes-2026-07-01", "inline-tools-2026-09-15",
    "mid-conversation-system-clear-at-2026-08-21", "dangerous-tool-use-2026-09-03",
    "advisor-tool-2026-03-01", "advanced-tool-use-2025-11-20", "effort-2025-11-24",
    "server-side-fallback-2026-06-01", "fallback-credit-2026-06-01", "structured-outputs-2025-12-15",
    "thinking-display-updates-2026-08-18", "thinking-binding-controls-2026-08-01",
    "thinking-resumption-2026-07-17", "extended-cache-ttl-2025-04-11", "prompt-caching-evict-2026-05-12",
    "cache-diagnosis-2026-04-07", "redact-thinking-2026-02-12", "afk-mode-2026-01-31",
    "interleaved-thinking-2025-05-14", "thinking-token-count-2026-05-13",
    "context-management-2025-06-27", "prompt-caching-scope-2026-01-05", "prompt-caching-2024-07-31",
];

const BODY_ORDER: &[&str] = &[
    "model", "messages", "system", "tools", "tool_choice", "metadata", "max_tokens", "thinking",
    "context_management", "output_config", "diagnostics", "stream", "fallbacks",
];

pub fn is_direct_base(upstream_override: Option<&str>) -> bool {
    match upstream_override {
        None => true,
        Some(base) => reqwest::Url::parse(base)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.eq_ignore_ascii_case("api.anthropic.com")))
            .unwrap_or(false),
    }
}

pub fn messages_url(base: Option<&str>) -> String {
    match base {
        Some(b) => match reqwest::Url::parse(b) {
            Ok(mut u) => {
                u.set_path("/v1/messages");
                u.set_query(Some("beta=true"));
                u.to_string()
            }
            Err(_) => "https://api.anthropic.com/v1/messages?beta=true".to_string(),
        },
        None => "https://api.anthropic.com/v1/messages?beta=true".to_string(),
    }
}

pub struct Identity<'a> {
    pub account_key: &'a str,
    pub account_uuid: &'a str,
    pub session_affinity: Option<&'a str>,
    pub device_id: &'a str,
}

impl<'a> Identity<'a> {
    pub fn new(
        account_key: &'a str,
        account_uuid: &'a str,
        session_affinity: Option<&'a str>,
        device_id: &'a str,
    ) -> Self {
        Self {
            account_key,
            account_uuid,
            session_affinity,
            device_id,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NativeProfile {
    pub user_agent: Option<String>,
    pub session_id: Option<String>,
    pub prompt_id: Option<String>,
    pub request_class: Option<String>,
    pub client_request_id: Option<String>,
    pub entrypoint: Option<String>,
    pub turn_origin: Option<String>,
}

impl NativeProfile {
    pub fn from_headers(headers: &http::HeaderMap) -> Self {
        Self {
            user_agent: headers.get("user-agent").and_then(|v| v.to_str().ok()).map(str::to_string),
            session_id: headers.get("x-claude-code-session-id").and_then(|v| v.to_str().ok()).map(str::to_string),
            prompt_id: headers.get("x-claude-code-prompt-id").and_then(|v| v.to_str().ok()).map(str::to_string),
            request_class: headers.get("x-claude-code-request-class").and_then(|v| v.to_str().ok()).map(str::to_string),
            client_request_id: headers.get("x-client-request-id").and_then(|v| v.to_str().ok()).map(str::to_string),
            entrypoint: None,
            turn_origin: None,
        }
    }
}

pub struct Wire {
    pub body: Vec<u8>,
    pub headers: Vec<(String, String)>,
}

pub fn is_subagent(headers: &http::HeaderMap) -> bool {
    headers.contains_key("x-claude-code-agent-id") || headers.contains_key("x-claude-code-parent-agent-id")
}

pub fn build(
    body: Value,
    id: &Identity<'_>,
    client_betas: Option<&str>,
    subagent: bool,
) -> Result<Wire, String> {
    build_with_native_profile(body, id, client_betas, subagent, None)
}

pub fn build_with_headers(
    body: Value,
    id: &Identity<'_>,
    client_headers: Option<&http::HeaderMap>,
    subagent: bool,
) -> Result<Wire, String> {
    let client_betas = client_headers.and_then(|h| h.get("anthropic-beta")).and_then(|v| v.to_str().ok());
    let native = client_headers.filter(|headers| {
        let (entrypoint, _, prompt) = parse_existing_billing(body.get("system"));
        let session = parse_existing_session_id(body.get("metadata"));
        let header_session = headers.get("x-claude-code-session-id").and_then(|v| v.to_str().ok());
        let header_prompt = headers.get("x-claude-code-prompt-id").and_then(|v| v.to_str().ok());
        matches!(entrypoint.as_deref(), Some("cli" | "sdk-cli"))
            && session.as_deref().is_some_and(|s| Some(s) == header_session)
            && prompt.as_deref().is_some_and(|p| Some(p) == header_prompt)
    }).map(NativeProfile::from_headers);
    let mut wire = build_with_native_profile(body, id, client_betas, subagent, native.as_ref())?;
    if native.is_some() {
        if let Some(headers) = client_headers {
            for (name, value) in headers {
                let name = name.as_str();
                if name == "anthropic-beta" || name.starts_with("x-stainless-")
                    || name.starts_with("x-claude-code-")
                {
                    let value = value.to_str().map_err(|error| error.to_string())?;
                    if let Some((_, existing)) = wire.headers.iter_mut().find(|(key, _)| key == name) {
                        *existing = value.to_string();
                    } else {
                        wire.headers.push((name.to_string(), value.to_string()));
                    }
                }
            }
        }
    }
    Ok(wire)
}

pub fn preserve_native_bytes(wire: &mut Wire, original: &[u8]) -> Result<(), String> {
    let Some(incoming_hash) = cch_offset(original) else { return Ok(()) };
    let Some(outgoing_hash) = cch_offset(&wire.body) else { return Ok(()) };
    let mut normalized_incoming = original.to_vec();
    let mut normalized_outgoing = wire.body.clone();
    normalized_incoming[incoming_hash..incoming_hash + 5].copy_from_slice(b"00000");
    normalized_outgoing[outgoing_hash..outgoing_hash + 5].copy_from_slice(b"00000");
    let incoming: Value = serde_json::from_slice(&normalized_incoming).map_err(|error| error.to_string())?;
    let outgoing: Value = serde_json::from_slice(&normalized_outgoing).map_err(|error| error.to_string())?;
    if incoming == outgoing {
        sign_cch(&mut normalized_incoming)?;
        wire.body = normalized_incoming;
    }
    Ok(())
}

pub fn build_with_native_profile(
    mut body: Value,
    id: &Identity<'_>,
    client_betas: Option<&str>,
    subagent: bool,
    native: Option<&NativeProfile>,
) -> Result<Wire, String> {
    if !body.is_object() {
        return Err("Claude request body is not an object".to_string());
    }
    let model = canonical_model(body["model"].as_str().unwrap_or_default());
    let probe = is_probe(&body);
    let requested: Vec<String> = client_betas
        .unwrap_or_default()
        .split(',')
        .map(|b| b.trim().to_string())
        .filter(|b| !b.is_empty())
        .collect();
    let has = |b: &str| requested.iter().any(|r| r == b);

    let original_billing = native.and_then(|_| body.get("system")?.as_array()?.iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .find(|text| text.starts_with(BILLING_PREFIX)).map(str::to_owned));
    let obj = body.as_object_mut().expect("checked object");
    obj.remove("prompt_cache_options");
    let thinking_type = obj.get("thinking").and_then(|t| t["type"].as_str()).unwrap_or_default().to_string();
    let thinking_active = matches!(thinking_type.as_str(), "enabled" | "adaptive" | "auto");
    obj.remove("temperature");
    obj.remove("top_p");
    if thinking_active {
        obj.remove("top_k");
    }
    if matches!(thinking_type.as_str(), "enabled" | "adaptive") {
        if let Some(thinking) = obj.get_mut("thinking").and_then(Value::as_object_mut) {
            thinking.entry("display").or_insert_with(|| json!("omitted"));
        }
    }
    if thinking_active && !obj.contains_key("output_config") {
        obj.insert("output_config".into(), json!({"effort": "medium"}));
    }
    if matches!(thinking_type.as_str(), "enabled" | "adaptive") && !obj.contains_key("context_management") {
        obj.insert("context_management".into(), json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}));
    }

    let legacy = LEGACY_MODELS.contains(&model.as_str());
    let (existing_entrypoint, existing_turn_origin, existing_prompt_id) = if native.is_some() {
        parse_existing_billing(obj.get("system"))
    } else {
        (None, None, None)
    };

    let entrypoint = native
        .and_then(|n| n.entrypoint.as_deref())
        .or(existing_entrypoint.as_deref())
        .or_else(|| {
            native.and_then(|n| n.user_agent.as_deref()).and_then(|ua| {
                if ua.contains("(external, cli)") {
                    Some("cli")
                } else {
                    None
                }
            })
        })
        .unwrap_or("sdk-cli");

    let is_cli = entrypoint == "cli";

    let turn_origin = if subagent {
        "sdk"
    } else if let Some(to) = native.and_then(|n| n.turn_origin.as_deref()) {
        to
    } else if let Some(to) = existing_turn_origin.as_deref() {
        to
    } else if is_cli {
        "human"
    } else {
        "sdk"
    };

    let prompt_id = native
        .and_then(|n| n.prompt_id.as_deref())
        .map(str::to_string)
        .or(existing_prompt_id)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let session_id = native
        .and_then(|n| n.session_id.as_deref())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            if let Some(affinity) = id.session_affinity.filter(|a| !a.is_empty()) {
                derive_uuid(&format!("session:{}:{}", id.account_key, affinity))
            } else {
                uuid::Uuid::new_v4().to_string()
            }
        });

    let client_request_id = native
        .and_then(|n| n.client_request_id.as_deref())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let request_class = native
        .and_then(|n| n.request_class.as_deref())
        .filter(|s| !s.is_empty())
        .unwrap_or("main")
        .to_string();

    let user_agent = native
        .and_then(|n| n.user_agent.as_deref())
        .filter(|ua| ua.starts_with("claude-cli/"))
        .map(str::to_string)
        .unwrap_or_else(|| {
            if is_cli {
                USER_AGENT_CLI.to_string()
            } else {
                USER_AGENT_SDK_CLI.to_string()
            }
        });

    let fingerprint_text = first_user_text(obj);
    let mut billing = format!(
        "{BILLING_PREFIX} cc_version={CLI_VERSION}.{}; cc_entrypoint={entrypoint}; cch=00000; cc_prompt_id={prompt_id}; cc_turn_origin={turn_origin};",
        fingerprint(&fingerprint_text, CLI_VERSION)
    );
    let first_sdk_turn = entrypoint == "sdk-cli" && !subagent
        && obj.get("messages").and_then(Value::as_array).is_some_and(|messages| {
            messages.iter().filter(|message| message["role"] == "user").count() == 1
                && messages.iter().all(|message| {
                    message["role"] != "assistant"
                        && !message["content"].as_array().is_some_and(|blocks|
                            blocks.iter().any(|block| block["type"] == "tool_result"))
                })
        });
    if first_sdk_turn {
        billing.push_str(" cc_prompt_index=0; cc_turn_index=1;");
    }
    if subagent {
        billing.push_str(" cc_is_subagent=true;");
    }
    if let Some(original) = original_billing {
        billing = original;
    }

    let billing_block = json!({"type": "text", "text": billing});
    match obj.remove("system") {
        Some(Value::Array(blocks)) => {
            let mut new_blocks = Vec::with_capacity(blocks.len() + 1);
            new_blocks.push(billing_block);
            for b in blocks {
                let is_old_billing = b.get("text").and_then(Value::as_str).is_some_and(|t| {
                    t.starts_with(BILLING_PREFIX)
                });
                if !is_old_billing {
                    new_blocks.push(b);
                }
            }
            obj.insert("system".into(), Value::Array(new_blocks));
        }
        Some(Value::String(s)) => {
            let s_trimmed = s.trim();
            if s_trimmed.starts_with(BILLING_PREFIX) {
                obj.insert("system".into(), json!([billing_block]));
            } else if !s_trimmed.is_empty() {
                obj.insert("system".into(), json!([billing_block, {"type": "text", "text": s}]));
            } else {
                obj.insert("system".into(), json!([billing_block]));
            }
        }
        Some(other) => {
            obj.insert("system".into(), json!([billing_block, other]));
        }
        None => {
            obj.insert("system".into(), json!([billing_block]));
        }
    }

    if !probe && !subagent {
        upgrade_ttl(&mut body);
    }
    enforce_cache_limit(&mut body, 4);

    let user_id = format!(
        "{{\"device_id\":{},\"account_uuid\":{},\"session_id\":{}}}",
        Value::from(id.device_id),
        Value::from(id.account_uuid),
        Value::from(session_id.clone())
    );
    let metadata = body.as_object_mut()
        .ok_or_else(|| "Claude request body is not an object".to_string())?
        .entry("metadata").or_insert_with(|| json!({}));
    if let Some(m) = metadata.as_object_mut() {
        m.insert("user_id".to_string(), json!(user_id));
    }

    let betas = compute_betas(&body, &model, legacy, probe, subagent, &requested, &has);
    let mut bytes = serialize_ordered(&body);
    sign_cch(&mut bytes)?;

    let headers = vec![
        ("authorization".to_string(), String::new()),
        ("content-type".to_string(), "application/json".to_string()),
        ("user-agent".to_string(), user_agent),
        ("x-claude-code-session-id".to_string(), session_id),
        ("x-stainless-arch".to_string(), "x64".to_string()),
        ("x-stainless-lang".to_string(), "js".to_string()),
        ("x-stainless-os".to_string(), "Linux".to_string()),
        ("x-stainless-package-version".to_string(), "0.128.0".to_string()),
        ("x-stainless-retry-count".to_string(), "0".to_string()),
        ("x-stainless-runtime".to_string(), "node".to_string()),
        ("x-stainless-runtime-version".to_string(), "v26.12.1".to_string()),
        ("x-stainless-timeout".to_string(), "600".to_string()),
        ("anthropic-beta".to_string(), betas),
        ("anthropic-dangerous-direct-browser-access".to_string(), "true".to_string()),
        ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ("x-app".to_string(), "cli".to_string()),
        ("x-claude-code-prompt-id".to_string(), prompt_id),
        ("x-claude-code-request-class".to_string(), request_class),
        ("x-client-request-id".to_string(), client_request_id),
        ("accept".to_string(), "application/json".to_string()),
        ("accept-encoding".to_string(), "gzip, deflate, br, zstd".to_string()),
    ];

    Ok(Wire { body: bytes, headers })
}

fn parse_existing_billing(system: Option<&Value>) -> (Option<String>, Option<String>, Option<String>) {
    let Some(items) = system.and_then(Value::as_array) else { return (None, None, None) };
    for item in items {
        if let Some(text) = item.get("text").and_then(Value::as_str) {
            if text.starts_with(BILLING_PREFIX) {
                let ep = extract_billing_field(text, "cc_entrypoint=");
                let to = extract_billing_field(text, "cc_turn_origin=");
                let pid = extract_billing_field(text, "cc_prompt_id=");
                return (ep, to, pid);
            }
        }
    }
    (None, None, None)
}

fn extract_billing_field(text: &str, field_prefix: &str) -> Option<String> {
    let start = text.find(field_prefix)? + field_prefix.len();
    let end = text[start..].find(';')? + start;
    Some(text[start..end].trim().to_string())
}

fn parse_existing_session_id(metadata: Option<&Value>) -> Option<String> {
    let user_id_str = metadata?.get("user_id")?.as_str()?;
    let parsed: Value = serde_json::from_str(user_id_str).ok()?;
    parsed.get("session_id")?.as_str().filter(|s| !s.is_empty()).map(str::to_string)
}

fn compute_betas(body: &Value, model: &str, legacy: bool, probe: bool, subagent: bool, requested: &[String], has: &dyn Fn(&str) -> bool) -> String {
    let mut betas: Vec<&str> = vec![
        "claude-code-20250219",
        "oauth-2025-04-20",
        "interleaved-thinking-2025-05-14",
        "context-management-2025-06-27",
        "prompt-caching-scope-2026-01-05",
        "thinking-token-count-2026-05-13",
    ];
    if has("context-1m-2025-08-07") {
        betas.push("context-1m-2025-08-07");
    }
    let per_turn = has("per-turn-control-2026-07-01") || model == "claude-opus-5-5" || model.starts_with("claude-fable-5-1");
    if !legacy {
        if has("mid-conversation-system-2026-04-07") {
            betas.push("mid-conversation-system-2026-04-07");
        }
        if per_turn {
            betas.push("per-turn-control-2026-07-01");
        }
        if !(model == "claude-sonnet-5" || (model.starts_with("claude-sonnet-5-") && !model.starts_with("claude-sonnet-5-5"))) {
            betas.push("mid-conversation-tool-changes-2026-07-01");
        }
    } else if per_turn {
        betas.push("per-turn-control-2026-07-01");
    }
    let thinking_type = body["thinking"]["type"].as_str().unwrap_or_default();
    if !probe && !model.contains("haiku") && thinking_type != "disabled" {
        betas.push("effort-2025-11-24");
    }
    betas.push("thinking-binding-controls-2026-08-01");
    betas.push("cache-diagnosis-2026-04-07");
    let fallbacks = body.get("fallbacks").is_some();
    if !probe && (fallbacks || has("server-side-fallback-2026-06-01")) {
        betas.push("server-side-fallback-2026-06-01");
    }
    if fallbacks || has("fallback-credit-2026-06-01") || body.get("fallback_credit_token").is_some() {
        betas.push("fallback-credit-2026-06-01");
    }
    if has("structured-outputs-2025-12-15") {
        betas.push("structured-outputs-2025-12-15");
    }
    let display = body["thinking"]["display"].as_str().unwrap_or_default().trim().to_string();
    if !probe && thinking_type != "disabled" && (has("thinking-display-updates-2026-08-18") || display.eq_ignore_ascii_case("updates")) {
        betas.push("thinking-display-updates-2026-08-18");
    }
    if body["speed"].as_str().is_some_and(|s| s.eq_ignore_ascii_case("fast")) || has("fast-mode-2026-02-01") {
        betas.push("fast-mode-2026-02-01");
    }
    if !probe && (!subagent || has("extended-cache-ttl-2025-04-11")) {
        betas.push("extended-cache-ttl-2025-04-11");
    }
    let mut out: Vec<String> = betas.into_iter().map(str::to_string).collect();
    for beta in requested {
        if !MANAGED_BETAS.contains(&beta.as_str()) && !out.contains(beta) {
            out.push(beta.clone());
        }
    }
    out.join(",")
}

fn canonical_model(model: &str) -> String {
    let m = model.trim().to_ascii_lowercase();
    m.rsplit('/').next().unwrap_or_default().to_string()
}

fn is_probe(body: &Value) -> bool {
    if body["max_tokens"].as_i64() != Some(1) || body["tools"].as_array().is_some_and(|t| !t.is_empty()) {
        return false;
    }
    let Some(messages) = body["messages"].as_array() else { return true };
    if messages.is_empty() {
        return true;
    }
    if messages.len() != 1 || messages[0]["role"] != "user" {
        return false;
    }
    let probe_word = |t: &str| matches!(t.trim(), "quota" | "test" | "." | "probe");
    match &messages[0]["content"] {
        Value::String(s) => probe_word(s),
        Value::Array(parts) => {
            let real: Vec<&str> = parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .filter(|t| !t.contains("<system-reminder>"))
                .collect();
            real.len() == 1 && probe_word(real[0])
        }
        _ => false,
    }
}

pub fn fingerprint(text: &str, version: &str) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let sampled: Vec<u16> = [4usize, 7, 20].iter().map(|&i| units.get(i).copied().unwrap_or(b'0' as u16)).collect();
    let picked = String::from_utf16_lossy(&sampled);
    let digest = crate::request_history::sha256(format!("{FINGERPRINT_SALT}{picked}{version}").as_bytes());
    hex(&digest)[..3].to_string()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn derive_uuid(seed: &str) -> String {
    let d = crate::request_history::sha256(seed.as_bytes());
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(b).to_string()
}

fn is_reminder(text: &str) -> bool {
    text.starts_with("<system-reminder>") && text.contains("</system-reminder>")
}

fn first_user_index(obj: &Map<String, Value>) -> Option<usize> {
    obj.get("messages")?.as_array()?.iter().position(|m| m["role"] == "user")
}

fn first_user_text(obj: &Map<String, Value>) -> String {
    let Some(idx) = first_user_index(obj) else { return String::new() };
    match &obj["messages"][idx]["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .find(|t| !is_reminder(t))
            .unwrap_or_default()
            .to_string(),
        _ => String::new(),
    }
}

fn for_each_cache_control(body: &mut Value, f: &mut dyn FnMut(&mut Map<String, Value>) -> bool) {
    for key in ["system", "tools"] {
        if let Some(items) = body.get_mut(key).and_then(Value::as_array_mut) {
            for item in items {
                if let Some(cc) = item.get_mut("cache_control").and_then(Value::as_object_mut) {
                    if !f(cc) {
                        item.as_object_mut().expect("object").remove("cache_control");
                    }
                }
            }
        }
    }
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for m in messages {
            if let Some(blocks) = m.get_mut("content").and_then(Value::as_array_mut) {
                for b in blocks {
                    if let Some(cc) = b.get_mut("cache_control").and_then(Value::as_object_mut) {
                        if !f(cc) {
                            b.as_object_mut().expect("object").remove("cache_control");
                        }
                    }
                }
            }
        }
    }
}

fn upgrade_ttl(body: &mut Value) {
    let mut short_ttl_seen = false;
    for_each_cache_control(body, &mut |cc| {
        short_ttl_seen |= cc.get("ttl").and_then(Value::as_str) == Some("5m");
        if !short_ttl_seen && cc.get("ttl").is_none() && cc.get("type").and_then(Value::as_str).is_some() {
            cc.insert("ttl".into(), json!("1h"));
        }
        true
    });
}

fn enforce_cache_limit(body: &mut Value, max: usize) {
    let mut total: usize = 0;
    for_each_cache_control(body, &mut |_| {
        total += 1;
        true
    });
    let mut excess = total.saturating_sub(max);
    let mut seen = 0;
    let keep_system_identity = 1;
    for_each_cache_control(body, &mut |_| {
        seen += 1;
        if excess > 0 && seen > keep_system_identity {
            excess -= 1;
            return false;
        }
        true
    });
}

fn key_rank(key: &str, top: bool) -> (usize, String) {
    if top {
        return (BODY_ORDER.iter().position(|k| *k == key).unwrap_or(BODY_ORDER.len()), key.to_string());
    }
    let nested = ["type", "role", "id", "name", "text", "content", "input", "tool_use_id", "description", "input_schema"];
    match nested.iter().position(|k| *k == key) {
        Some(p) => (p, String::new()),
        None if key == "cache_control" => (usize::MAX, String::new()),
        None => (nested.len(), key.to_string()),
    }
}

fn write_value(out: &mut Vec<u8>, value: &Value, top: bool) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_by_key(|k| key_rank(k, top));
            out.push(b'{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend(serde_json::to_vec(key).expect("string"));
                out.push(b':');
                write_value(out, &map[key], false);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(out, item, false);
            }
            out.push(b']');
        }
        other => out.extend(serde_json::to_vec(other).expect("scalar")),
    }
}

fn serialize_ordered(body: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write_value(&mut out, body, true);
    out
}

fn cch_offset(body: &[u8]) -> Option<usize> {
    let marker = b"\"text\":\"x-anthropic-billing-header:";
    let start = body.windows(marker.len()).position(|w| w == marker)?;
    let end = start + body[start + marker.len()..].iter().position(|&b| b == b'"')? + marker.len();
    let field = &body[start..end];
    let rel = field.windows(4).position(|w| w == b"cch=")?;
    let digits = start + rel + 4;
    (body.get(digits + 5) == Some(&b';') && body[digits..digits + 5].iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c)))
        .then_some(digits)
}

pub fn sign_cch(body: &mut [u8]) -> Result<(), String> {
    let Some(offset) = cch_offset(body) else { return Ok(()) };
    body[offset..offset + 5].copy_from_slice(b"00000");
    let normalized = normalize_cch(body)?;
    let hash = xxhash_rust::xxh64::xxh64(&normalized, CCH_SEED);
    body[offset..offset + 5].copy_from_slice(format!("{:05x}", hash & 0xFFFFF).as_bytes());
    Ok(())
}

struct Scanner<'a> {
    b: &'a [u8],
    pos: usize,
    edits: Vec<(usize, usize)>,
}

struct Member {
    start: usize,
    end: usize,
    comma_before: Option<usize>,
    comma_after: Option<usize>,
    excluded: bool,
}

fn normalize_cch(body: &[u8]) -> Result<Vec<u8>, String> {
    let mut s = Scanner { b: body, pos: 0, edits: Vec::new() };
    s.value(true)?;
    s.ws();
    if s.pos != body.len() {
        return Err(format!("unexpected JSON data at byte {}", s.pos));
    }
    s.edits.sort();
    let mut out = Vec::with_capacity(body.len());
    let mut last = 0;
    for (start, end) in s.edits {
        if start < last {
            return Err("overlapping CCH edit".into());
        }
        out.extend_from_slice(&body[last..start]);
        last = end;
    }
    out.extend_from_slice(&body[last..]);
    Ok(out)
}

impl Scanner<'_> {
    fn ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\t' | b'\r' | b'\n') {
            self.pos += 1;
        }
    }
    fn eat(&mut self, c: u8) -> bool {
        if self.b.get(self.pos) == Some(&c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn edit(&mut self, start: usize, end: usize) {
        if start < end {
            self.edits.push((start, end));
        }
    }
    fn string(&mut self) -> Result<(usize, usize), String> {
        if self.b.get(self.pos) != Some(&b'"') {
            return Err(format!("missing JSON string at byte {}", self.pos));
        }
        let start = self.pos;
        self.pos += 1;
        while self.pos < self.b.len() {
            match self.b[self.pos] {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return Ok((start, self.pos));
                }
                _ => self.pos += 1,
            }
        }
        Err("unterminated JSON string".into())
    }
    fn value(&mut self, collect: bool) -> Result<(), String> {
        self.ws();
        match self.b.get(self.pos) {
            None => Err("missing JSON value".into()),
            Some(b'{') => self.object(collect),
            Some(b'[') => {
                self.pos += 1;
                self.ws();
                if self.eat(b']') {
                    return Ok(());
                }
                loop {
                    self.value(collect)?;
                    self.ws();
                    if self.eat(b',') {
                        continue;
                    }
                    return if self.eat(b']') { Ok(()) } else { Err("missing array end".into()) };
                }
            }
            Some(b'"') => self.string().map(|_| ()),
            Some(_) => {
                let start = self.pos;
                while self.pos < self.b.len() && !matches!(self.b[self.pos], b',' | b'}' | b']' | b' ' | b'\t' | b'\r' | b'\n') {
                    self.pos += 1;
                }
                if self.pos == start { Err("missing JSON value".into()) } else { Ok(()) }
            }
        }
    }
    fn object(&mut self, collect: bool) -> Result<(), String> {
        self.pos += 1;
        self.ws();
        if self.eat(b'}') {
            return Ok(());
        }
        let mut members = Vec::new();
        let mut comma_before = None;
        loop {
            self.ws();
            let start = self.pos;
            let (ks, ke) = self.string()?;
            self.ws();
            if !self.eat(b':') {
                return Err("missing object colon".into());
            }
            self.ws();
            let key = &self.b[ks..ke];
            let excluded = collect && matches!(key, b"\"max_tokens\"" | b"\"fallbacks\"" | b"\"fallback_credit_token\"");
            if collect && key == b"\"model\"" && self.b.get(self.pos) == Some(&b'"') {
                let (vs, ve) = self.string()?;
                self.edit(vs + 1, ve - 1);
            } else {
                self.value(collect && !excluded)?;
            }
            let end = self.pos;
            self.ws();
            let comma_after = if self.eat(b',') { Some(self.pos - 1) } else { None };
            members.push(Member { start, end, comma_before, comma_after, excluded });
            if comma_after.is_some() {
                comma_before = comma_after;
                continue;
            }
            if !self.eat(b'}') {
                return Err("missing object end".into());
            }
            break;
        }
        if collect {
            let mut i = 0;
            while i < members.len() {
                if !members[i].excluded {
                    i += 1;
                    continue;
                }
                let mut j = i;
                while j + 1 < members.len() && members[j + 1].excluded {
                    j += 1;
                }
                let (s, e) = if j + 1 < members.len() {
                    (members[i].start, members[j].comma_after.expect("comma") + 1)
                } else if i > 0 && j > i {
                    (members[i].start, members[j].end)
                } else if i > 0 {
                    (members[i].comma_before.expect("comma"), members[j].end)
                } else {
                    (members[i].start, members[j].end)
                };
                self.edit(s, e);
                i = j + 1;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"{"model":"model-a","messages":[{"role":"user","content":[{"type":"text","text":"x"}]}],"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"},{"type":"text","text":"system-x"}],"tools":[],"metadata":{"user_id":"meta-x"},"max_tokens":1,"thinking":{"type":"adaptive","display":"omitted"},"context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]},"output_config":{"effort":"high"},"stream":true}"#;

    fn cch(body: &str) -> String {
        let mut bytes = body.as_bytes().to_vec();
        sign_cch(&mut bytes).unwrap();
        let off = cch_offset(&bytes).unwrap();
        String::from_utf8(bytes[off..off + 5].to_vec()).unwrap()
    }

    #[test]
    fn cch_reproduces_official_2_1_316_tool_roundtrip_bytes() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/claude_official_tool_roundtrip.json")).unwrap();
        for request in fixture["requests"].as_array().unwrap() {
            let raw = request["raw"].as_str().unwrap();
            let offset = cch_offset(raw.as_bytes()).unwrap();
            assert_eq!(cch(raw), &raw[offset..offset + 5]);
        }
    }

    #[test]
    fn cch_matches_the_reference_vectors() {
        let r = |from: &str, to: &str| BASE.replacen(from, to, 1);
        assert_eq!(cch(BASE), "7ee87");
        assert_eq!(cch(&r(r#""model":"model-a""#, r#""model":"model-b""#)), "7ee87");
        assert_eq!(cch(&r(r#""max_tokens":1"#, r#""max_tokens":2"#)), "7ee87");
        assert_eq!(cch(&r(r#""text":"x""#, r#""text":"y""#)), "b9cc8");
        assert_eq!(cch(&r(r#""stream":true}"#, r#""stream":true,"fallbacks":[{"model":"fallback-a"}]}"#)), "7ee87");
        assert_eq!(cch(&r(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","max_tokens":999,"fallbacks":[{"model":"fallback-model"}]}"#)), "4589b");
        assert_eq!(cch(&r(r#""metadata":{"user_id":"meta-x"}"#, r#""metadata":{"user_id":"meta-x","max_tokens":999,"model":"nested-model","fallbacks":[{"model":"fallback-model"}]}"#)), "0601b");
        assert_eq!(cch(&r(r#""tools":[]"#, r#""tools":[{"name":"t","description":"d","input_schema":{"type":"object"}}]"#)), "3d78d");
        assert_eq!(cch(r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.test; cc_entrypoint=sdk-cli; cch=00000;"}]}"#), "f2edb");
    }

    #[test]
    fn native_roundtrip_preserves_official_attribution() {
        let fixture: Value = serde_json::from_str(include_str!("../tests/fixtures/claude_official_two_turns.json")).unwrap();
        for request in fixture["requests"].as_array().unwrap() {
            let body: Value = serde_json::from_str(request["raw"].as_str().unwrap()).unwrap();
            let original = body["system"][0]["text"].as_str().unwrap().to_owned();
            let mut headers = http::HeaderMap::new();
            for (name, value) in request["headers"].as_object().unwrap() {
                headers.insert(http::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    http::HeaderValue::from_str(value.as_str().unwrap()).unwrap());
            }
            let metadata: Value = serde_json::from_str(body["metadata"]["user_id"].as_str().unwrap()).unwrap();
            let identity = Identity::new("fixture", "", None, metadata["device_id"].as_str().unwrap());
            let original_body = body.clone();
            let mut wire = build_with_headers(body, &identity, Some(&headers), false).unwrap();
            preserve_native_bytes(&mut wire, request["raw"].as_str().unwrap().as_bytes()).unwrap();
            for (name, expected) in &headers {
                let name = name.as_str();
                if name == "anthropic-beta" || name.starts_with("x-stainless-") || name.starts_with("x-claude-code-") {
                    assert_eq!(wire.headers.iter().find(|(key, _)| key == name).unwrap().1, expected.to_str().unwrap());
                }
            }
            let output: Value = serde_json::from_slice(&wire.body).unwrap();
            let actual = output["system"][0]["text"].as_str().unwrap();
            let without_hash = |text: &str| text.split(';').filter(|field| !field.trim().starts_with("cch=")).map(str::to_owned).collect::<Vec<_>>();
            assert_eq!(without_hash(actual), without_hash(&original));
            let mut comparable = output.clone();
            comparable["system"][0]["text"] = original_body["system"][0]["text"].clone();
            assert_eq!(comparable, original_body);
            assert_eq!(wire.body.as_slice(), request["raw"].as_str().unwrap().as_bytes(),
                "native request serialization must preserve the captured bytes");
        }
    }

    #[test]
    fn only_api_anthropic_com_is_direct() {
        assert!(is_direct_base(None));
        assert!(is_direct_base(Some("https://api.anthropic.com")));
        assert!(!is_direct_base(Some("https://ccapi.labs.mengmota.com/anthropic")));
        assert!(!is_direct_base(Some("http://127.0.0.1:9")));
    }

    #[test]
    fn fingerprint_matches_the_official_capture() {
        assert_eq!(fingerprint("CPA_OFFICIAL_BASEURL_CLI_SYSTEM_EMPTY_b82d4e", "2.1.258"), "1f4");
        assert_eq!(fingerprint("Reply LOCAL_OK.", "2.1.316"), "959");
    }

    #[test]
    fn messages_url_preserves_custom_port() {
        assert_eq!(messages_url(None), "https://api.anthropic.com/v1/messages?beta=true");
        assert_eq!(
            messages_url(Some("https://api.anthropic.com:8443")),
            "https://api.anthropic.com:8443/v1/messages?beta=true"
        );
        assert_eq!(
            messages_url(Some("https://ccapi.labs.mengmota.com/anthropic")),
            "https://ccapi.labs.mengmota.com/v1/messages?beta=true"
        );
    }

    #[test]
    fn build_preserves_tools_system_and_isolates_sessions() {
        let body = json!({
            "model": "claude-opus-5-5", "max_tokens": 1000, "stream": true,
            "system": "be terse", "temperature": 0.2,
            "thinking": {"type": "adaptive"},
            "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "hello world, this is a test"}],
        });
        let id = Identity::new("claude-a", "u-1", Some("affinity-s"), "device-1234567890abcdef");
        let wire = build(body, &id, Some("prompt-caching-2024-07-31,future-beta-2027-01-01"), false).unwrap();
        let text = String::from_utf8(wire.body.clone()).unwrap();
        assert!(text.starts_with(r#"{"model":"claude-opus-5-5","messages":"#), "{text}");
        let v: Value = serde_json::from_slice(&wire.body).unwrap();
        assert!(v["system"][0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header: cc_version=2.1.316."));
        assert_eq!(v["system"][1]["text"], "be terse");
        assert_eq!(v["tools"][0]["name"], "Read");
        assert!(v.get("fallbacks").is_none());
        assert_eq!(v["thinking"]["display"], "omitted");
        assert_eq!(v["output_config"]["effort"], "medium");
        assert!(v.get("temperature").is_none());
        let user_id: Value = serde_json::from_str(v["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(user_id["account_uuid"], "u-1");
        assert_eq!(user_id["device_id"], "device-1234567890abcdef");
        let mut unsigned = wire.body.clone();
        let off = cch_offset(&unsigned).unwrap();
        let sent = String::from_utf8(unsigned[off..off + 5].to_vec()).unwrap();
        sign_cch(&mut unsigned).unwrap();
        assert_eq!(String::from_utf8(unsigned[off..off + 5].to_vec()).unwrap(), sent);
        let betas = &wire.headers.iter().find(|(k, _)| k == "anthropic-beta").unwrap().1;
        assert!(betas.starts_with("claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14"), "{betas}");
        assert!(betas.ends_with("future-beta-2027-01-01"));
        let ua = &wire.headers.iter().find(|(k, _)| k == "user-agent").unwrap().1;
        assert_eq!(ua, USER_AGENT_SDK_CLI);
        let os = &wire.headers.iter().find(|(k, _)| k == "x-stainless-os").unwrap().1;
        assert_eq!(os, "Linux");
        let arch = &wire.headers.iter().find(|(k, _)| k == "x-stainless-arch").unwrap().1;
        assert_eq!(arch, "x64");
    }

    #[test]
    fn build_with_headers_preserves_native_cli_profile() {
        let body = json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 1000,
            "system": [{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli; cch=00000; cc_turn_origin=human; cc_prompt_id=prompt-native-456;"}],
            "metadata":{"user_id":"{\"session_id\":\"session-native-123\"}"},
            "messages": [{"role": "user", "content": "test prompt"}],
        });
        let mut headers = http::HeaderMap::new();
        headers.insert("user-agent", http::HeaderValue::from_static("claude-cli/2.1.316 (external, cli)"));
        headers.insert("x-claude-code-session-id", http::HeaderValue::from_static("session-native-123"));
        headers.insert("x-claude-code-prompt-id", http::HeaderValue::from_static("prompt-native-456"));
        headers.insert("x-client-request-id", http::HeaderValue::from_static("req-native-789"));
        let id = Identity::new("claude-a", "u-2", None, "device-native");
        let wire = build_with_headers(body, &id, Some(&headers), false).unwrap();
        let v: Value = serde_json::from_slice(&wire.body).unwrap();
        let billing = v["system"][0]["text"].as_str().unwrap();
        assert!(billing.contains("cc_entrypoint=cli;"));
        assert!(billing.contains("cc_turn_origin=human;"));
        assert!(billing.contains("cc_prompt_id=prompt-native-456;"));
        let ua = &wire.headers.iter().find(|(k, _)| k == "user-agent").unwrap().1;
        assert_eq!(ua, "claude-cli/2.1.316 (external, cli)");
        let sid = &wire.headers.iter().find(|(k, _)| k == "x-claude-code-session-id").unwrap().1;
        assert_eq!(sid, "session-native-123");
        let pid = &wire.headers.iter().find(|(k, _)| k == "x-claude-code-prompt-id").unwrap().1;
        assert_eq!(pid, "prompt-native-456");
        let req_id = &wire.headers.iter().find(|(k, _)| k == "x-client-request-id").unwrap().1;
        assert_eq!(req_id, "req-native-789");
    }

    #[test]
    fn session_isolation_without_affinity() {
        let body1 = json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 1000,
            "messages": [{"role": "user", "content": "turn one"}],
        });
        let body2 = json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 1000,
            "messages": [{"role": "user", "content": "turn two"}],
        });
        let id = Identity::new("claude-a", "u-3", None, "device-isolated");
        let wire1 = build(body1, &id, None, false).unwrap();
        let wire2 = build(body2, &id, None, false).unwrap();
        let sid1 = &wire1.headers.iter().find(|(k, _)| k == "x-claude-code-session-id").unwrap().1;
        let sid2 = &wire2.headers.iter().find(|(k, _)| k == "x-claude-code-session-id").unwrap().1;
        assert_ne!(sid1, sid2);
    }

    #[test]
    fn synthesized_history_does_not_invent_turn_position() {
        let body = json!({"model":"claude-opus-5-5","messages":[
            {"role":"user","content":"first"},
            {"role":"assistant","content":"response"},
            {"role":"user","content":"next"}
        ]});
        let identity = Identity::new("fixture", "", Some("session"), "device");
        let wire = build(body, &identity, None, false).unwrap();
        let body: Value = serde_json::from_slice(&wire.body).unwrap();
        let billing = body["system"][0]["text"].as_str().unwrap();
        assert!(extract_billing_field(billing, "cc_turn_index=").is_none());
        assert!(extract_billing_field(billing, "cc_prompt_index=").is_none());
    }
}

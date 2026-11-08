use axum::http::HeaderMap;
use serde_json::Value;

pub struct PayloadRuleContext<'a> {
    pub model: &'a str,
    pub requested_model: &'a str,
    pub protocol: &'a str,
    pub from_protocol: &'a str,
    pub headers: &'a HeaderMap,
    pub original_body: &'a Value,
}

fn matches_model_rule(rule: &Value, ctx: &PayloadRuleContext<'_>, current_payload: &Value) -> bool {
    if let Some(name) = rule.get("name").and_then(Value::as_str) {
        if name != "*" && name != ctx.model && name != ctx.requested_model {
            return false;
        }
    }

    if let Some(proto) = rule.get("protocol").and_then(Value::as_str) {
        if !proto.is_empty() && proto != "*" && proto != ctx.protocol {
            return false;
        }
    }

    if let Some(from_proto) = rule.get("from-protocol").and_then(Value::as_str) {
        if !from_proto.is_empty() && from_proto != "*" && from_proto != ctx.from_protocol {
            return false;
        }
    }

    if let Some(headers) = rule.get("headers").and_then(Value::as_object) {
        for (k, v) in headers {
            if let Some(expected_val) = v.as_str() {
                let actual = ctx
                    .headers
                    .get(k)
                    .and_then(|h| h.to_str().ok())
                    .unwrap_or_default();
                if actual != expected_val {
                    return false;
                }
            }
        }
    }

    if let Some(matches) = rule.get("match").and_then(Value::as_array) {
        for m in matches {
            if let Some(m_obj) = m.as_object() {
                for (k, v) in m_obj {
                    let actual = get_path(current_payload, k);
                    if actual != Some(v) {
                        return false;
                    }
                }
            }
        }
    }

    if let Some(not_matches) = rule.get("not-match").and_then(Value::as_array) {
        for m in not_matches {
            if let Some(m_obj) = m.as_object() {
                for (k, v) in m_obj {
                    let actual = get_path(current_payload, k);
                    if actual == Some(v) {
                        return false;
                    }
                }
            }
        }
    }

    if let Some(exists) = rule.get("exist").and_then(Value::as_array) {
        for path in exists {
            if let Some(p_str) = path.as_str() {
                if get_path(current_payload, p_str).is_none() {
                    return false;
                }
            }
        }
    }

    if let Some(not_exists) = rule.get("not-exist").and_then(Value::as_array) {
        for path in not_exists {
            if let Some(p_str) = path.as_str() {
                if get_path(current_payload, p_str).is_some() {
                    return false;
                }
            }
        }
    }

    true
}

fn get_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    for part in path.split('.') {
        match current {
            Value::Object(map) => {
                current = map.get(part)?;
            }
            Value::Array(arr) => {
                let idx: usize = part.parse().ok()?;
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

fn set_path(value: &mut Value, path: &str, new_val: Value) {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.is_empty() {
        return;
    }
    let mut current = value;
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            if let Value::Object(ref mut map) = current {
                map.insert((*part).to_string(), new_val);
            }
            return;
        }
        if !current.is_object() {
            *current = Value::Object(serde_json::Map::new());
        }
        if let Value::Object(ref mut map) = current {
            current = map
                .entry((*part).to_string())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
        }
    }
}

fn remove_path(value: &mut Value, path: &str) {
    let parts: Vec<&str> = path.split('.').collect();
    if parts.is_empty() {
        return;
    }
    let mut current = value;
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            if let Value::Object(ref mut map) = current {
                map.remove(*part);
            }
            return;
        }
        match current {
            Value::Object(ref mut map) => {
                if let Some(next) = map.get_mut(*part) {
                    current = next;
                } else {
                    return;
                }
            }
            _ => return,
        }
    }
}

/// `-raw` rules hold raw JSON text (upstream SetRawBytes): the parsed JSON is
/// written, not the string. Settings validation rejects unparsable text, so a
/// fallback to the literal is unreachable for a published configuration.
fn rule_value(kind: &str, value: &Value) -> Value {
    if kind.ends_with("-raw") {
        if let Some(parsed) = value
            .as_str()
            .and_then(|raw| serde_json::from_str(raw).ok())
        {
            return parsed;
        }
    }
    value.clone()
}

pub fn apply_payload_rules(
    payload_config: &serde_json::Map<String, Value>,
    target_body: &mut Value,
    ctx: &PayloadRuleContext<'_>,
) {
    for key in &["default", "default-raw"] {
        if let Some(rules) = payload_config.get(*key).and_then(Value::as_array) {
            for rule in rules {
                let matches = rule
                    .get("models")
                    .and_then(Value::as_array)
                    .map(|models| {
                        models
                            .iter()
                            .any(|m| matches_model_rule(m, ctx, target_body))
                    })
                    .unwrap_or(false);
                if matches {
                    if let Some(params) = rule.get("params").and_then(Value::as_object) {
                        for (k, v) in params {
                            if get_path(target_body, k).is_none()
                                && get_path(ctx.original_body, k).is_none()
                            {
                                set_path(target_body, k, rule_value(key, v));
                            }
                        }
                    }
                }
            }
        }
    }

    for key in &["override", "override-raw"] {
        if let Some(rules) = payload_config.get(*key).and_then(Value::as_array) {
            for rule in rules {
                let matches = rule
                    .get("models")
                    .and_then(Value::as_array)
                    .map(|models| {
                        models
                            .iter()
                            .any(|m| matches_model_rule(m, ctx, target_body))
                    })
                    .unwrap_or(false);
                if matches {
                    if let Some(params) = rule.get("params").and_then(Value::as_object) {
                        for (k, v) in params {
                            set_path(target_body, k, rule_value(key, v));
                        }
                    }
                }
            }
        }
    }

    if let Some(rules) = payload_config.get("filter").and_then(Value::as_array) {
        for rule in rules {
            let matches = rule
                .get("models")
                .and_then(Value::as_array)
                .map(|models| {
                    models
                        .iter()
                        .any(|m| matches_model_rule(m, ctx, target_body))
                })
                .unwrap_or(false);
            if matches {
                if let Some(params) = rule.get("params").and_then(Value::as_array) {
                    for path in params {
                        if let Some(p_str) = path.as_str() {
                            remove_path(target_body, p_str);
                        }
                    }
                }
            }
        }
    }
}

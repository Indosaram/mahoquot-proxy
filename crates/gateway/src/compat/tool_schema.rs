//! Outbound Claude tool-schema compatibility, not a general JSON Schema validator.
//!
//! `required` is optional in draft 2020-12. The explicit object shape below is
//! for Claude/Antigravity tool validators; it must not make optional fields
//! mandatory. Valid direct-Anthropic constraints ($defs, additionalProperties,
//! unions, etc.) are deliberately retained. Gemini transport cleanup is separate.

use serde_json::{json, Map, Value};

const MAX_SCHEMA_DEPTH: usize = 64;
const MAX_EXPANDED_NODES: usize = 8192;

pub(super) fn anthropic_schema(parameters: Option<&Value>) -> Result<Value, String> {
    let mut schema = parameters.cloned().unwrap_or_else(|| json!({}));
    normalize_root(&mut schema)?;
    Ok(schema)
}

pub(super) fn antigravity_claude_schema(parameters: Option<&Value>) -> Result<Value, String> {
    let root = parameters.cloned().unwrap_or_else(|| json!({}));
    let mut schema = root.clone();
    let mut remaining = MAX_EXPANDED_NODES;
    // Resolve before removing definition blocks. Otherwise the Gemini sanitizer
    // leaves dangling $refs which fail Anthropic's downstream schema validation.
    inline_refs(&mut schema, &root, &mut Vec::new(), &mut remaining, 0)?;
    normalize_root(&mut schema)?;
    Ok(schema)
}

fn normalize_root(schema: &mut Value) -> Result<(), String> {
    if schema.is_null() || *schema == Value::Bool(true) {
        *schema = json!({});
    }
    let map = schema
        .as_object_mut()
        .ok_or("tool input_schema must be an object schema")?;
    match map.get("type") {
        None | Some(Value::Null) => {
            map.insert("type".into(), json!("object"));
        }
        Some(Value::String(kind)) if kind == "object" => {}
        _ => return Err("tool input_schema type must be object".into()),
    }
    normalize_node(schema, 0)
}

fn normalize_node(schema: &mut Value, depth: usize) -> Result<(), String> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err("tool input_schema exceeds the schema nesting limit".into());
    }
    if schema.is_boolean() {
        return Ok(());
    }
    let map = schema
        .as_object_mut()
        .ok_or("schema nodes must be objects or booleans")?;
    if map.contains_key("properties") && map.get("type").is_none_or(Value::is_null) {
        map.insert("type".into(), json!("object"));
    }
    let object_type = map.get("type").is_some_and(|kind| {
        kind == "object"
            || kind
                .as_array()
                .is_some_and(|types| types.iter().any(|kind| kind == "object"))
    });
    if object_type || map.contains_key("properties") {
        map.entry("properties").or_insert_with(|| json!({}));
        if map.get("required").is_none_or(Value::is_null) {
            map.insert("required".into(), json!([]));
        }
    }
    if let Some(required) = map.get_mut("required") {
        let names = required
            .as_array_mut()
            .ok_or("required must be an array of strings")?;
        if names.iter().any(|name| !name.is_string()) {
            return Err("required must contain only strings".into());
        }
        // Duplicate entries are invalid in draft 2020-12; removing duplicates
        // preserves the constraint without filtering names against properties.
        let mut seen = std::collections::HashSet::new();
        names.retain(|name| seen.insert(name.clone()));
    }
    visit_schema_children(map, &mut |child| normalize_node(child, depth + 1))
}

/// Visit schema positions only, never property names or literal data in
/// default/examples/enum/const. A generic JSON walk corrupts both of those.
pub(super) fn visit_schema_children(
    map: &mut Map<String, Value>,
    visit: &mut impl FnMut(&mut Value) -> Result<(), String>,
) -> Result<(), String> {
    for key in [
        "properties",
        "patternProperties",
        "$defs",
        "definitions",
        "dependentSchemas",
    ] {
        if let Some(children) = map.get_mut(key) {
            let children = children
                .as_object_mut()
                .ok_or_else(|| format!("{key} must be a map of schemas"))?;
            for child in children.values_mut() {
                visit(child)?;
            }
        }
    }
    for key in [
        "items",
        "additionalProperties",
        "additionalItems",
        "contains",
        "not",
        "if",
        "then",
        "else",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contentSchema",
    ] {
        if let Some(child) = map.get_mut(key) {
            visit(child)?;
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = map.get_mut(key) {
            let children = children
                .as_array_mut()
                .ok_or_else(|| format!("{key} must be an array of schemas"))?;
            for child in children {
                visit(child)?;
            }
        }
    }
    // Legacy dependencies may contain either schemas or arrays of names.
    if let Some(children) = map.get_mut("dependencies").and_then(Value::as_object_mut) {
        for child in children.values_mut().filter(|child| !child.is_array()) {
            visit(child)?;
        }
    }
    Ok(())
}

fn inline_refs(
    schema: &mut Value,
    root: &Value,
    stack: &mut Vec<String>,
    remaining: &mut usize,
    depth: usize,
) -> Result<(), String> {
    if depth > MAX_SCHEMA_DEPTH || *remaining == 0 {
        return Err("Antigravity tool schema reference expansion limit exceeded".into());
    }
    *remaining -= 1;
    if depth > 0 && schema.get("$id").is_some() {
        // A nested resource changes the base of fragment refs. Do not resolve
        // those against the wrong root document after Gemini strips $id.
        return Err("Antigravity cannot inline schemas with nested $id resources".into());
    }
    if let Some(reference) = schema.get("$ref") {
        let reference = reference
            .as_str()
            .ok_or("$ref must be a string")?
            .to_string();
        let pointer = reference
            .strip_prefix('#')
            .filter(|pointer| pointer.is_empty() || pointer.starts_with('/'))
            .ok_or_else(|| {
                format!("Antigravity requires a local JSON Pointer $ref: {reference}")
            })?;
        if stack.contains(&reference) {
            return Err(format!(
                "Antigravity cannot inline recursive $ref: {reference}"
            ));
        }
        let mut target = root
            .pointer(pointer)
            .cloned()
            .ok_or_else(|| format!("unresolved tool schema $ref: {reference}"))?;
        stack.push(reference.clone());
        inline_refs(&mut target, root, stack, remaining, depth + 1)?;
        stack.pop();
        if target == Value::Bool(true) {
            target = json!({});
        }
        let target_map = target
            .as_object_mut()
            .ok_or_else(|| format!("Antigravity cannot inline non-object schema at {reference}"))?;
        if let Some(siblings) = schema.as_object() {
            for (key, value) in siblings {
                if key == "$ref" {
                    continue;
                }
                // Annotation siblings may override annotations. Conflicting
                // validation siblings require an intersection, not last-write
                // wins; reject rather than silently weaken the tool contract.
                let annotation = matches!(
                    key.as_str(),
                    "title"
                        | "description"
                        | "default"
                        | "examples"
                        | "$comment"
                        | "$schema"
                        | "$id"
                );
                if !annotation && target_map.get(key).is_some_and(|old| old != value) {
                    return Err(format!(
                        "cannot inline $ref {reference} with conflicting {key}"
                    ));
                }
                target_map.insert(key.clone(), value.clone());
            }
        }
        *schema = target;
    }
    if let Some(map) = schema.as_object_mut() {
        if map.contains_key("$dynamicRef") || map.contains_key("$recursiveRef") {
            return Err("Antigravity cannot inline dynamic or recursive schema references".into());
        }
        // The immutable root still owns all definitions needed by local refs.
        // Do not expand unused definitions (which may themselves be recursive).
        map.remove("$defs");
        map.remove("definitions");
        visit_schema_children(map, &mut |child| {
            inline_refs(child, root, stack, remaining, depth + 1)
        })?;
    }
    Ok(())
}

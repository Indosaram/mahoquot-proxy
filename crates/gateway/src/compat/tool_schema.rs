//! Outbound Claude tool-schema compatibility, not a general JSON Schema validator.
//!
//! `required` is optional in draft 2020-12. The explicit object shape below is
//! for Claude/Antigravity tool validators; it must not make optional fields
//! mandatory. Verified against the live Vertex-hosted Anthropic validator
//! (2026-09-16 probes): `allOf`, non-recursive `$defs`/`$ref`,
//! `patternProperties`, `propertyNames`, `const`, `default` (incl. null),
//! and `format` are accepted, but `anyOf`/`oneOf` unions are rejected with
//! "tools.N.custom.input_schema: JSON schema is invalid ... draft 2020-12"
//! even though unions are valid draft 2020-12. Unions are therefore flattened
//! before dispatch. Gemini transport cleanup is separate.

use serde_json::{json, Map, Value};

const MAX_SCHEMA_DEPTH: usize = 64;
const MAX_EXPANDED_NODES: usize = 8192;

const UNION_KEYS: [&str; 2] = ["anyOf", "oneOf"];

pub(super) fn anthropic_schema(parameters: Option<&Value>) -> Result<Value, String> {
    let mut schema = parameters.cloned().unwrap_or_else(|| json!({}));
    flatten_unions(&mut schema, 0)?;
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
    flatten_unions(&mut schema, 0)?;
    normalize_root(&mut schema)?;
    Ok(schema)
}

fn is_null_branch(branch: &Value) -> bool {
    branch.as_object().and_then(|map| map.get("type")) == Some(&json!("null"))
}

/// True when the schema is an `anyOf`/`oneOf` union with at least one `null`
/// branch, i.e. the strict-conversion shape for an optional field.
fn union_has_null_branch(schema: &Value) -> bool {
    let Some(map) = schema.as_object() else {
        return false;
    };
    UNION_KEYS.iter().any(|key| {
        map.get(*key)
            .and_then(Value::as_array)
            .is_some_and(|branches| branches.iter().any(is_null_branch))
    })
}

const ANNOTATION_KEYS: [&str; 5] = ["title", "description", "default", "examples", "$comment"];

/// Merge the union node's annotations onto a chosen branch without overriding
/// anything the branch already declares.
fn overlay_annotations(reduced: &mut Map<String, Value>, union_node: &Map<String, Value>) {
    for key in ANNOTATION_KEYS {
        if let Some(value) = union_node.get(key) {
            reduced.entry(key.to_string()).or_insert_with(|| value.clone());
        }
    }
}

fn is_object_schema(branch: &Value) -> bool {
    branch.as_object().is_some_and(|map| {
        map.contains_key("properties")
            || map.get("type").is_some_and(|kind| kind == "object")
    })
}

/// Reduce an already-recursively-flattened branch list to one schema.
fn reduce_union(branches: Vec<Value>, union_node: &Map<String, Value>) -> Value {
    let non_null: Vec<Value> = branches.into_iter().filter(|b| !is_null_branch(b)).collect();
    let mut reduced = match non_null.len() {
        0 => json!({}),
        1 => non_null.into_iter().next().unwrap_or_else(|| json!({})),
        _ if non_null.iter().all(is_object_schema) => {
            let mut properties = Map::new();
            let mut required: Option<Vec<String>> = None;
            let mut first_rest: Option<Map<String, Value>> = None;
            for branch in &non_null {
                let map = branch.as_object().expect("checked object schema");
                if let Some(props) = map.get("properties").and_then(Value::as_object) {
                    for (name, prop) in props {
                        properties
                            .entry(name.clone())
                            .or_insert_with(|| prop.clone());
                    }
                }
                // A branch without `required` declares nothing mandatory, so
                // the merged object can require only what every branch does.
                let branch_required: Vec<String> = map
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|names| {
                        names
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                required = Some(match required {
                    None => branch_required,
                    Some(mut kept) => {
                        kept.retain(|name| branch_required.contains(name));
                        kept
                    }
                });
                if first_rest.is_none() {
                    first_rest = Some(map.clone());
                }
            }
            let mut merged = Map::new();
            if let Some(first) = first_rest {
                for (key, value) in first {
                    if key != "properties"
                        && key != "required"
                        && !UNION_KEYS.contains(&key.as_str())
                    {
                        merged.insert(key.clone(), value.clone());
                    }
                }
            }
            merged.insert("type".into(), json!("object"));
            merged.insert("properties".into(), Value::Object(properties));
            merged.insert(
                "required".into(),
                json!(required.unwrap_or_default()),
            );
            Value::Object(merged)
        }
        _ => non_null.into_iter().next().unwrap_or_else(|| json!({})),
    };
    if let Some(map) = reduced.as_object_mut() {
        overlay_annotations(map, union_node);
    }
    reduced
}

/// Replace every `anyOf`/`oneOf` union with a single equivalent schema. The
/// strict Vertex-hosted Anthropic tool validator rejects unions outright.
/// Nullable unions (the universal optional-field encoding) become the non-null
/// branch and the property is dropped from the parent's `required` list;
/// all-object unions merge; mixed unions keep the first branch.
fn flatten_unions(schema: &mut Value, depth: usize) -> Result<(), String> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err("tool input_schema exceeds the schema nesting limit".into());
    }
    let Some(map) = schema.as_object_mut() else {
        return Ok(()); // booleans and non-objects carry no unions we repair
    };
    // Record nullable-union property names before recursion mutates them.
    let nullable_names: Vec<String> = map
        .get("properties")
        .and_then(Value::as_object)
        .map(|props| {
            props
                .iter()
                .filter(|(_, prop)| union_has_null_branch(prop))
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default();
    // Recurse first so nested unions inside branches and child positions are
    // already reduced when the branch-level merge below runs.
    visit_schema_children(map, &mut |child| flatten_unions(child, depth + 1))?;
    if !nullable_names.is_empty() {
        if let Some(required) = map.get_mut("required").and_then(Value::as_array_mut) {
            required.retain(|name| {
                !nullable_names
                    .iter()
                    .any(|nullable| name == nullable)
            });
        }
    }
    for key in UNION_KEYS {
        if let Some(branches) = map.get(key).and_then(Value::as_array).cloned() {
            // Only the first present union key is reduced; a node carrying
            // both anyOf and oneOf is not a meaningful schema.
            let reduced = reduce_union(branches, map);
            match reduced {
                Value::Object(reduced_map) => *map = reduced_map,
                _ => *map = Map::new(),
            }
            break;
        }
    }
    Ok(())
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

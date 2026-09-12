use mahoquot_gateway::compat::claude::{anthropic_to_openai, openai_to_anthropic};
use mahoquot_gateway::compat::gemini::{openai_to_antigravity, openai_to_gemini};
use mahoquot_gateway::compat::openai_to_codex;
use serde_json::{json, Value};

const CLAUDE_MODEL: &str = "claude-opus-4-6-thinking";

// Match the zero-based tools.14.custom.input_schema location in the reported
// upstream error, without any real account, project, or network dependency.
fn tools_14_request(schema: Value) -> Value {
    let mut tools: Vec<Value> = (0..14)
        .map(|index| {
            json!({
                "name": format!("tool_{index}"),
                "description": "Unrelated valid tool",
                "input_schema": {"type": "object", "properties": {}, "required": []}
            })
        })
        .collect();
    tools.push(json!({
        "name": "tool_14",
        "description": "Schema repair regression",
        "input_schema": schema
    }));
    let inbound = json!({
        "model": CLAUDE_MODEL,
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "Use tool_14"}],
        "tools": tools
    });
    let openai = anthropic_to_openai(&inbound).expect("normalize inbound request");
    assert_eq!(openai["tools"][14]["function"]["parameters"], schema);
    openai
}

fn direct_schema(request: &Value) -> Value {
    openai_to_anthropic(request).expect("translate")["tools"][14]["input_schema"].clone()
}

fn bridge_schema(request: &Value) -> Value {
    openai_to_antigravity(request, "test-project").expect("translate")["request"]["tools"][0]
        ["functionDeclarations"][14]["parameters"]
        .clone()
}

fn assert_explicit_object(schema: &Value) {
    assert_eq!(schema["type"], "object", "tool schema: {schema}");
    assert_eq!(schema["required"], json!([]), "tool schema: {schema}");
    assert_eq!(schema["properties"]["query"]["type"], "string");
}

#[test]
fn tools_14_antigravity_claude_repairs_missing_object_fields() {
    let request = tools_14_request(json!({
        "properties": {"query": {"type": "string"}}
    }));
    let original = request.clone();
    let out = openai_to_antigravity(&request, "test-project").expect("translate");
    let declarations = out["request"]["tools"][0]["functionDeclarations"]
        .as_array()
        .expect("15 declarations");
    assert_eq!(declarations.len(), 15);
    assert_eq!(declarations[14]["name"], "tool_14");
    assert_explicit_object(&declarations[14]["parameters"]);
    assert_eq!(out["model"], CLAUDE_MODEL);
    assert_eq!(out["project"], "test-project");
    assert_eq!(request, original, "conversion must not mutate the input");
}

#[test]
fn tools_14_direct_anthropic_repairs_missing_object_fields() {
    let request = tools_14_request(json!({
        "properties": {"query": {"type": "string"}}
    }));
    let out = openai_to_anthropic(&request).expect("translate");
    assert_eq!(out["tools"].as_array().unwrap().len(), 15);
    assert_eq!(out["tools"][14]["name"], "custom_tool_14");
    assert_explicit_object(&out["tools"][14]["input_schema"]);
}

#[test]
fn tools_14_antigravity_claude_resolves_defs_before_sanitizing() {
    let request = tools_14_request(json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {"Query": {"type": "string", "minLength": 1}},
        "type": "object",
        "properties": {"query": {"$ref": "#/$defs/Query"}},
        "required": ["query"],
        "additionalProperties": false
    }));
    let schema = bridge_schema(&request);
    assert_eq!(schema["properties"]["query"]["type"], "string");
    assert_eq!(schema["properties"]["query"]["minLength"], 1);
    assert_eq!(schema["required"], json!(["query"]));
    assert!(schema["properties"]["query"].get("$ref").is_none());
    for key in ["$schema", "$defs", "additionalProperties"] {
        assert!(schema.get(key).is_none(), "bridge must not emit {key}");
    }
}

#[test]
fn tools_14_existing_object_type_still_gets_required_array() {
    let request = tools_14_request(json!({
        "type": "object", "properties": {"query": {"type": "string"}}
    }));
    assert_explicit_object(&direct_schema(&request));
    assert_explicit_object(&bridge_schema(&request));
}

#[test]
fn tools_14_nested_objects_arrays_and_unions_preserve_optional_fields() {
    let request = tools_14_request(json!({
        "properties": {
            "options": {"properties": {"enabled": {"type": "boolean"}}},
            "rows": {"type": "array", "items": {
                "properties": {"label": {"type": "string"}, "count": {"type": "integer"}},
                "required": ["label"]
            }},
            "choice": {"anyOf": [
                {"properties": {"tag": {"type": "string"}}},
                {"type": "null"}
            ]}
        },
        "required": ["rows"]
    }));
    for schema in [direct_schema(&request), bridge_schema(&request)] {
        assert_eq!(schema["required"], json!(["rows"]));
        assert_eq!(schema["properties"]["options"]["type"], "object");
        assert_eq!(schema["properties"]["options"]["required"], json!([]));
        assert_eq!(
            schema["properties"]["options"]["properties"]["enabled"]["type"],
            "boolean"
        );
        let rows = &schema["properties"]["rows"];
        assert_eq!(rows["type"], "array");
        assert!(rows.get("required").is_none());
        assert_eq!(rows["items"]["type"], "object");
        assert_eq!(rows["items"]["required"], json!(["label"]));
        assert_eq!(rows["items"]["properties"]["count"]["type"], "integer");
        let choice = &schema["properties"]["choice"]["anyOf"];
        assert_eq!(choice[0]["required"], json!([]));
        assert_eq!(choice[1], json!({"type": "null"}));
    }
}

#[test]
fn tools_14_direct_anthropic_preserves_valid_constraints_and_literal_data() {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {"Query": {"type": "string", "minLength": 1}},
        "type": "object",
        "properties": {
            "query": {"$ref": "#/$defs/Query"},
            "kind": {"const": "search", "default": "search"},
            "literal": {"default": {"properties": {"type": 7}, "required": "data"}},
            "enum_data": {"enum": [{"properties": {}, "$ref": "not-a-schema"}]}
        },
        "required": ["query"],
        "additionalProperties": false,
        "patternProperties": {"^x-": {"type": "string"}},
        "propertyNames": {"pattern": "^[a-z_-]+$"},
        "allOf": [{"minProperties": 1}],
        "examples": [{"properties": {}, "required": "literal"}]
    });
    assert_eq!(direct_schema(&tools_14_request(schema.clone())), schema);
}

#[test]
fn tools_14_bridge_keeps_keyword_named_properties_and_translates_const() {
    let names = [
        "const",
        "default",
        "examples",
        "$schema",
        "$defs",
        "definitions",
        "additionalProperties",
        "properties",
        "required",
        "type",
    ];
    let properties: serde_json::Map<String, Value> = names
        .iter()
        .map(|name| (name.to_string(), json!({"type": "string"})))
        .collect();
    let mut schema = json!({"properties": properties, "required": ["const"]});
    schema["properties"]["const"]["const"] = json!("fixed");
    let repaired = bridge_schema(&tools_14_request(schema));
    for name in names {
        assert_eq!(
            repaired["properties"][name]["type"], "string",
            "lost property {name}"
        );
    }
    assert_eq!(repaired["properties"]["const"]["enum"], json!(["fixed"]));
    assert!(repaired["properties"]["const"].get("const").is_none());
    assert_eq!(repaired["required"], json!(["const"]));
}

#[test]
fn tools_14_legacy_definitions_chains_escaped_pointers_and_siblings() {
    let request = tools_14_request(json!({
        "definitions": {
            "name/a~b": {"type": "string", "description": "base", "minLength": 1},
            "Alias": {"$ref": "#/definitions/name~1a~0b"}
        },
        "properties": {
            "query": {"$ref": "#/definitions/Alias", "description": "specific"},
            "other": {"$ref": "#/definitions/Alias"}
        }
    }));
    let schema = bridge_schema(&request);
    assert!(schema.get("definitions").is_none());
    assert_eq!(
        schema["properties"]["query"],
        json!({
            "type": "string", "description": "specific", "minLength": 1
        })
    );
    assert_eq!(schema["properties"]["other"]["type"], "string");
}

#[test]
fn tools_14_root_ref_is_resolved_before_defaults_are_added() {
    let request = tools_14_request(json!({
        "$defs": {"Args": {
            "type": "object", "properties": {"query": {"type": "string"}},
            "required": ["query"]
        }},
        "$ref": "#/$defs/Args"
    }));
    assert_eq!(
        bridge_schema(&request),
        json!({
            "type": "object", "properties": {"query": {"type": "string"}},
            "required": ["query"]
        })
    );
}

#[test]
fn tools_14_bad_refs_fail_locally_with_index_instead_of_dangling_schemas() {
    for (schema, expected) in [
        (
            json!({"properties": {"query": {"$ref": "#/$defs/Missing"}}}),
            "unresolved",
        ),
        (
            json!({"properties": {"query": {"$ref": "https://example.invalid/schema"}}}),
            "local JSON Pointer",
        ),
        (
            json!({"$defs": {"Loop": {"$ref": "#/$defs/Loop"}}, "$ref": "#/$defs/Loop"}),
            "recursive",
        ),
        (
            json!({"$defs": {"Name": {"type": "string", "minLength": 5}},
            "properties": {"query": {"$ref": "#/$defs/Name", "minLength": 1}}}),
            "conflicting",
        ),
        (
            json!({"properties": {"query": {"$id": "nested", "type": "string"}}}),
            "nested $id",
        ),
    ] {
        let error = openai_to_antigravity(&tools_14_request(schema), "test-project")
            .expect_err("unsupported reference must fail locally");
        assert!(error.contains("tools[14].function.parameters"), "{error}");
        assert!(error.contains(expected), "{error}");
    }
}

#[test]
fn tools_14_direct_recursive_schema_is_not_flattened() {
    let schema = json!({
        "type": "object", "properties": {"head": {"$ref": "#/$defs/Node"}}, "required": [],
        "$defs": {"Node": {
            "type": "object", "properties": {"next": {"$ref": "#/$defs/Node"}}, "required": []
        }}
    });
    assert_eq!(direct_schema(&tools_14_request(schema.clone())), schema);
}

#[test]
fn tools_14_absent_null_empty_and_true_schemas_get_empty_object_defaults() {
    for value in [Value::Null, json!({}), json!(true)] {
        let request = tools_14_request(value);
        for repaired in [direct_schema(&request), bridge_schema(&request)] {
            assert_eq!(
                repaired,
                json!({"type": "object", "properties": {}, "required": []})
            );
        }
    }
    let mut request = tools_14_request(json!({}));
    request["tools"][14]["function"]
        .as_object_mut()
        .unwrap()
        .remove("parameters");
    assert_eq!(direct_schema(&request), bridge_schema(&request));
    assert_eq!(bridge_schema(&request)["required"], json!([]));
}

#[test]
fn tools_14_invalid_shapes_fail_locally_without_panicking() {
    for schema in [
        json!(false),
        json!(42),
        json!([]),
        json!({"type": "array"}),
        json!({"properties": []}),
        json!({"required": "query"}),
        json!({"required": [3]}),
    ] {
        let request = tools_14_request(schema);
        let direct = openai_to_anthropic(&request).expect_err("invalid schema");
        let bridge = openai_to_antigravity(&request, "test-project").expect_err("invalid schema");
        assert!(direct.contains("tools[14].function.parameters"), "{direct}");
        assert!(bridge.contains("tools[14].function.parameters"), "{bridge}");
    }
}

#[test]
fn tools_14_duplicate_required_names_are_repaired_without_dropping_constraints() {
    let request = tools_14_request(json!({
        "properties": {"query": {"type": "string"}},
        "required": ["query", "query", "undeclared"]
    }));
    for schema in [direct_schema(&request), bridge_schema(&request)] {
        assert_eq!(schema["required"], json!(["query", "undeclared"]));
    }
}

#[test]
fn tools_14_gemini_and_non_claude_antigravity_keep_existing_conversion() {
    let mut request = tools_14_request(json!({
        "properties": {"query": {"type": "string", "default": "hello"}},
        "additionalProperties": false
    }));
    for model in ["gemini-3.7-flash-high", "gpt-oss-120b-medium"] {
        request["model"] = json!(model);
        let native = openai_to_gemini(&request).expect("Gemini");
        let envelope = openai_to_antigravity(&request, "test-project").expect("Antigravity");
        assert_eq!(envelope["request"], native);
        assert_eq!(
            native["tools"][0]["functionDeclarations"][14]["parameters"],
            json!({"properties": {"query": {"type": "string"}}})
        );
    }
    request["model"] = json!(CLAUDE_MODEL);
    let native = openai_to_gemini(&request).expect("native converter stays unchanged");
    assert!(native["tools"][0]["functionDeclarations"][14]["parameters"]
        .get("required")
        .is_none());
}

#[test]
fn tools_14_codex_parameters_are_unchanged() {
    let schema = json!({
        "$defs": {"Query": {"type": "string"}},
        "properties": {"query": {"$ref": "#/$defs/Query"}},
        "additionalProperties": false
    });
    let mut request = tools_14_request(schema.clone());
    request["model"] = json!("gpt-5.4");
    let translated = openai_to_codex(&serde_json::to_vec(&request).unwrap()).expect("Codex");
    let body: Value = serde_json::from_slice(&translated.body).unwrap();
    assert_eq!(body["tools"][14]["parameters"], schema);
    assert_eq!(request["tools"][14]["function"]["parameters"], schema);
}

#[test]
fn tools_14_non_thinking_claude_is_repaired_and_normalization_is_idempotent() {
    let mut request = tools_14_request(json!({"properties": {"query": {"type": "string"}}}));
    request["model"] = json!("claude-sonnet-4-6");
    let bridge = bridge_schema(&request);
    assert_explicit_object(&bridge);
    assert_eq!(bridge_schema(&tools_14_request(bridge.clone())), bridge);
    let direct = direct_schema(&request);
    assert_eq!(direct_schema(&tools_14_request(direct.clone())), direct);
}

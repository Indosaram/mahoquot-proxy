use mahoquot_gateway::compat::claude::{
    anthropic_to_openai, estimate_input_tokens, messages_payload, render_anthropic_stream,
    stop_reason_for, openai_to_anthropic, anthropic_json_to_openai,
};
use mahoquot_gateway::compat::events::{CodexEvent, Usage};
use serde_json::json;

#[test]
fn native_anthropic_tool_names_are_not_treated_as_gateway_prefixes() {
    let request = json!({
        "model":"fixture",
        "messages":[{"role":"assistant","content":[
            {"type":"tool_use","id":"call_a","name":"custom_lookup","input":{}}
        ]}],
        "tools":[
            {"name":"lookup","input_schema":{"type":"object"}},
            {"name":"custom_lookup","input_schema":{"type":"object"}}
        ],
        "tool_choice":{"type":"tool","name":"custom_lookup"}
    });
    let converted = anthropic_to_openai(&request).expect("native request translation");
    assert_eq!(converted["tools"][0]["function"]["name"], "lookup");
    assert_eq!(converted["tools"][1]["function"]["name"], "custom_lookup");
    assert_eq!(converted["messages"][0]["tool_calls"][0]["function"]["name"], "custom_lookup");
    assert_eq!(converted["tool_choice"]["function"]["name"], "custom_lookup");
}

#[test]
fn test_t10_r24_r28_two_turn_wire_fixture_keeps_history_definition_and_choice_aligned() {
    let response = anthropic_json_to_openai(&json!({
        "id":"msg_fixture", "content":[{"type":"tool_use","id":"call_fixture",
            "name":"custom_lookup","input":{"city":"seoul"}}],
        "stop_reason":"tool_use", "usage":{"input_tokens":3,"output_tokens":2}
    }), "fixture", 1);
    let request = json!({
        "model":"fixture",
        "messages":[{"role":"user","content":"weather?"},
            response["choices"][0]["message"],
            {"role":"tool","tool_call_id":"call_fixture","content":"21C"},
            {"role":"user","content":"check again"}],
        "tools":[{"type":"function","function":{"name":"lookup","parameters":{"type":"object"}}}],
        "tool_choice":{"type":"function","function":{"name":"lookup"}},
        "parallel_tool_calls":false
    });
    let wire = openai_to_anthropic(&request).unwrap();
    assert_eq!(wire["messages"], json!([
        {"role":"user","content":[{"type":"text","text":"weather?"}]},
        {"role":"assistant","content":[{"type":"tool_use","id":"call_fixture","name":"custom_lookup","input":{"city":"seoul"}}]},
        {"role":"user","content":[{"type":"tool_result","tool_use_id":"call_fixture","content":"21C"}]},
        {"role":"user","content":[{"type":"text","text":"check again"}]}
    ]));
    assert_eq!(wire["tools"][0]["name"], "custom_lookup");
    assert_eq!(wire["tool_choice"], json!({"type":"tool","name":"custom_lookup","disable_parallel_tool_use":true}));
    let back = anthropic_to_openai(&wire).unwrap();
    assert_eq!(back["tools"][0]["function"]["name"], "custom_lookup");
    assert_eq!(back["messages"][1]["tool_calls"][0]["function"]["name"], "custom_lookup");
    assert_eq!(back["tool_choice"], json!({"type":"function","function":{"name":"custom_lookup"}}));
    assert_eq!(back["parallel_tool_calls"], false);
}

#[test]
fn test_t10_request_maps_system_and_messages() {
    let body = json!({
        "model": "gemini-3.7-flash-high",
        "max_tokens": 512,
        "system": "be terse",
        "messages": [{"role": "user", "content": "Say exactly: alpha bravo"}]
    });

    let out = anthropic_to_openai(&body).expect("translate");

    assert_eq!(out["model"], "gemini-3.7-flash-high");
    assert_eq!(out["messages"][0]["role"], "system");
    assert_eq!(out["messages"][0]["content"], "be terse");
    assert_eq!(out["messages"][1]["role"], "user");
    assert_eq!(out["messages"][1]["content"], "Say exactly: alpha bravo");
    assert_eq!(out["max_tokens"], 512);
}

#[test]
fn test_t10_block_content_and_tool_use_roundtrip() {
    let body = json!({
        "model": "claude-sonnet-4-6",
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "weather?"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather",
                 "input": {"city": "seoul"}}
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "21C"}
            ]}
        ],
        "tools": [{"name": "get_weather", "description": "w",
                   "input_schema": {"type": "object"}}]
    });

    let out = anthropic_to_openai(&body).expect("translate");

    assert_eq!(out["messages"][0]["content"], "weather?");

    let call = &out["messages"][1]["tool_calls"][0];
    assert_eq!(call["id"], "toolu_1");
    assert_eq!(call["function"]["name"], "get_weather");
    let args: serde_json::Value =
        serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["city"], "seoul");

    assert_eq!(out["messages"][2]["role"], "tool");
    assert_eq!(out["messages"][2]["tool_call_id"], "toolu_1");
    assert_eq!(out["messages"][2]["content"], "21C");

    assert_eq!(out["tools"][0]["type"], "function");
    assert_eq!(out["tools"][0]["function"]["name"], "get_weather");
    assert_eq!(out["tools"][0]["function"]["parameters"]["type"], "object");
}

#[test]
fn test_t10_response_shape_matches_anthropic() {
    let usage = Usage {
        prompt_tokens: 6,
        completion_tokens: 2,
        total_tokens: 8,
        cached_tokens: 0,
        reasoning_tokens: 0,
        cache_write_tokens: 0,
    };
    let payload = messages_payload(
        "msg_1",
        "gemini-3.7-flash-high",
        "alpha bravo",
        &[],
        "stop",
        Some(&usage),
        None,
    );

    assert_eq!(payload["type"], "message");
    assert_eq!(payload["role"], "assistant");
    assert_eq!(payload["content"][0]["type"], "text");
    assert_eq!(payload["content"][0]["text"], "alpha bravo");
    assert_eq!(payload["stop_reason"], "end_turn");
    assert!(payload["stop_sequence"].is_null());
    assert_eq!(payload["usage"]["input_tokens"], 6);
    assert_eq!(payload["usage"]["output_tokens"], 2);
}

#[test]
fn test_t10_thinking_block_precedes_text_when_signature_present() {
    let payload = messages_payload(
        "msg_2",
        "gemini-3.7-flash-high",
        "alpha bravo",
        &[],
        "stop",
        None,
        Some("sig-abc"),
    );

    assert_eq!(payload["content"][0]["type"], "thinking");
    assert_eq!(payload["content"][0]["signature"], "sig-abc");
    assert_eq!(payload["content"][1]["type"], "text");
    assert_eq!(payload["content"][1]["text"], "alpha bravo");
}

#[test]
fn test_t10_stop_reason_mapping() {
    assert_eq!(stop_reason_for("stop"), "end_turn");
    assert_eq!(stop_reason_for("length"), "max_tokens");
    assert_eq!(stop_reason_for("tool_calls"), "tool_use");
}

#[test]
fn test_t10_stream_emits_anthropic_event_sequence() {
    let events = vec![
        CodexEvent::TextDelta("alpha ".to_string()),
        CodexEvent::TextDelta("bravo".to_string()),
        CodexEvent::Completed {
            usage: Some(Usage {
                prompt_tokens: 6,
                completion_tokens: 2,
                total_tokens: 8,
                cached_tokens: 0,
                reasoning_tokens: 0,
                cache_write_tokens: 0,
            }),
        },
    ];

    let (frames, usage) = render_anthropic_stream(&events, "msg_1", "gemini-3.7-flash-high");
    let joined = frames.concat();

    let order: Vec<&str> = [
        "message_start",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "message_delta",
        "message_stop",
    ]
    .to_vec();
    let mut cursor = 0usize;
    for name in &order {
        let needle = format!("event: {name}\n");
        let found = joined[cursor..]
            .find(&needle)
            .unwrap_or_else(|| panic!("missing {name} after offset {cursor} in:\n{joined}"));
        cursor += found + needle.len();
    }

    assert!(joined.contains("\"text\":\"alpha \""));
    assert!(joined.contains("\"text\":\"bravo\""));

    let start = frames.first().expect("message_start frame");
    let start_json: serde_json::Value =
        serde_json::from_str(start.split_once("data: ").expect("data payload").1.trim())
            .expect("parse message_start");
    assert_eq!(
        start_json["message"]["usage"]["input_tokens"], 6,
        "message_start must carry real input tokens, not a placeholder zero"
    );
    assert!(
        !joined.contains("chat.completion"),
        "openai chunk shape must not leak into the anthropic surface"
    );
    assert!(
        !joined.contains("[DONE]"),
        "anthropic streams terminate with message_stop, not [DONE]"
    );
    assert_eq!(usage.map(|u| u.completion_tokens), Some(2));
}

#[test]
fn test_t10_count_tokens_is_positive_and_scales() {
    let small = json!({"messages": [{"role": "user", "content": "hi"}]});
    let large = json!({
        "system": "you are a helpful assistant that answers concisely",
        "messages": [{"role": "user", "content": "a".repeat(400)}]
    });

    let s = estimate_input_tokens(&small);
    let l = estimate_input_tokens(&large);

    assert!(s > 0, "must return a usable count, got {s}");
    assert!(l > s, "longer input must count higher: {l} vs {s}");
}

#[test]
fn test_t10_anthropic_usage_with_cache_creation_input_tokens_parses_into_cache_write_tokens() {
    let response = json!({
        "id": "msg_test",
        "content": [{"type": "text", "text": "cached response"}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 50,
            "output_tokens": 15,
            "cache_read_input_tokens": 30,
            "cache_creation_input_tokens": 20
        }
    });
    let openai = anthropic_json_to_openai(&response, "claude-3-5-sonnet", 1234);
    assert_eq!(openai["usage"]["prompt_tokens"], 50);
    assert_eq!(openai["usage"]["completion_tokens"], 15);
    assert_eq!(openai["usage"]["total_tokens"], 65);
    assert_eq!(openai["usage"]["cache_read_input_tokens"], 30);
    assert_eq!(openai["usage"]["cache_creation_input_tokens"], 20);
    assert_eq!(openai["usage"]["prompt_tokens_details"]["cached_tokens"], 30);
    assert_eq!(openai["usage"]["prompt_tokens_details"]["cache_write_tokens"], 20);
}


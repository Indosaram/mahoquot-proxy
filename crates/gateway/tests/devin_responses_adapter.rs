//! Devin Responses API request normalization and SSE event adapter tests.
//!
//! Exercises `compat::responses` in isolation via direct `#[path]` inclusion
//! while `crates/gateway/src/compat/mod.rs` remains untouched.

use bytes::Bytes;
use mahoquot_gateway::compat::events::{CodexEvent, Usage};
use serde_json::{json, Value};

pub mod compat {
    pub use mahoquot_gateway::compat::*;
}

// Bridge module path to test `compat/responses.rs` without modifying `compat/mod.rs`.
#[path = "../src/compat/responses.rs"]
mod responses;

use responses::{
    responses_response, responses_to_openai, ResponsesError, ResponsesStreamRenderer,
};

fn parse_sse_frames(frames: &[Bytes]) -> Vec<(String, Value)> {
    let mut results = Vec::new();
    for f in frames {
        let text = String::from_utf8_lossy(f);
        for block in text.split("\n\n") {
            let block = block.trim();
            if block.is_empty() {
                continue;
            }
            let mut event_type = String::new();
            let mut data_payload = String::new();
            for line in block.lines() {
                if let Some(ev) = line.strip_prefix("event: ") {
                    event_type = ev.trim().to_string();
                } else if let Some(dt) = line.strip_prefix("data: ") {
                    data_payload = dt.trim().to_string();
                }
            }
            if !data_payload.is_empty() {
                let json_val: Value = serde_json::from_str(&data_payload)
                    .unwrap_or_else(|e| panic!("invalid json in SSE frame '{data_payload}': {e}"));
                results.push((event_type, json_val));
            }
        }
    }
    results
}

// ─── 1. Request Normalization Tests ──────────────────────────────────────────

#[test]
fn test_request_instructions_and_string_input() {
    let req = json!({
        "model": "devin/glm-5-2",
        "instructions": "Be concise and accurate.",
        "input": "Explain quantum superposition."
    });

    let chat = responses_to_openai(&req).expect("valid conversion");
    assert_eq!(chat["model"], "devin/glm-5-2");
    let messages = chat["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "Be concise and accurate.");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "Explain quantum superposition.");
}

#[test]
fn test_request_structured_input_messages_and_parameters() {
    let req = json!({
        "model": "devin/glm-5-2",
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "First part, "}, {"type": "input_text", "text": "second part."}]
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Acknowledged."}]
            }
        ],
        "temperature": 0.7,
        "top_p": 0.9,
        "max_output_tokens": 1024
    });

    let chat = responses_to_openai(&req).expect("valid conversion");
    let messages = chat["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "First part, second part.");
    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["content"], "Acknowledged.");
    assert_eq!(chat["temperature"], 0.7);
    assert_eq!(chat["top_p"], 0.9);
    assert_eq!(chat["max_tokens"], 1024);
    assert!(chat.get("max_output_tokens").is_none());
}

#[test]
fn test_request_reasoning_signature_and_redaction_preservation() {
    let req = json!({
        "model": "devin/glm-5-2",
        "input": [
            {
                "type": "message",
                "role": "assistant",
                "content": [
                    {"type": "reasoning", "reasoning": "step-by-step logic", "signature": "sig-abc-123"},
                    {"type": "output_text", "text": "Final conclusion."}
                ],
                "reasoning_redacted": true
            }
        ]
    });

    let chat = responses_to_openai(&req).expect("valid conversion");
    let messages = chat["messages"].as_array().expect("messages array");
    assert_eq!(messages.len(), 1);
    let assistant = &messages[0];
    assert_eq!(assistant["role"], "assistant");
    assert_eq!(assistant["content"], "Final conclusion.");
    assert_eq!(assistant["reasoning_content"], "step-by-step logic");
    assert_eq!(assistant["reasoning_signature"], "sig-abc-123");
    assert_eq!(assistant["reasoning_redacted"], true);
}

#[test]
fn test_request_base64_vision_accepted_and_remote_url_rejected() {
    let valid_req = json!({
        "model": "devin/glm-5-2",
        "input": [
            {
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Inspect this:"},
                    {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo="}
                ]
            }
        ]
    });
    let chat = responses_to_openai(&valid_req).expect("base64 vision accepted");
    let parts = chat["messages"][0]["content"].as_array().expect("content parts");
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["type"], "text");
    assert_eq!(parts[1]["type"], "image_url");
    assert_eq!(parts[1]["image_url"]["url"], "data:image/png;base64,iVBORw0KGgo=");

    let remote_req = json!({
        "model": "devin/glm-5-2",
        "input": [
            {
                "role": "user",
                "content": [
                    {"type": "input_image", "image_url": "https://example.com/photo.png"}
                ]
            }
        ]
    });
    let err = responses_to_openai(&remote_req).expect_err("remote image URL must be rejected");
    assert!(err.to_string().contains("remote image url is unsupported"));
}

#[test]
fn test_request_unsupported_remote_stateful_and_background_features_rejected() {
    // 1. previous_response_id
    let req_prev_id = json!({
        "model": "devin/glm-5-2",
        "previous_response_id": "resp_previous_123",
        "input": "continue"
    });
    let err = responses_to_openai(&req_prev_id).expect_err("previous_response_id rejected");
    assert!(err.to_string().contains("previous_response_id is not supported"));

    // 2. background: true
    let req_background = json!({
        "model": "devin/glm-5-2",
        "background": true,
        "input": "long running task"
    });
    let err = responses_to_openai(&req_background).expect_err("background=true rejected");
    assert!(err.to_string().contains("background=true is not supported"));

    // 3. store: true
    let req_store = json!({
        "model": "devin/glm-5-2",
        "store": true,
        "input": "persist this"
    });
    let err = responses_to_openai(&req_store).expect_err("store=true rejected");
    assert!(err.to_string().contains("store=true is not supported"));
}

#[test]
fn test_request_function_tools_and_tool_choice_mapping() {
    let req = json!({
        "model": "devin/glm-5-2",
        "input": "What is the weather?",
        "tools": [
            {
                "type": "function",
                "name": "get_current_weather",
                "description": "Get weather for location",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    },
                    "required": ["location"]
                }
            }
        ],
        "tool_choice": {"type": "function", "name": "get_current_weather"}
    });

    let chat = responses_to_openai(&req).expect("valid tool conversion");
    let tools = chat["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0]["type"], "function");
    assert_eq!(tools[0]["function"]["name"], "get_current_weather");
    assert_eq!(tools[0]["function"]["description"], "Get weather for location");
    assert_eq!(chat["tool_choice"]["type"], "function");
    assert_eq!(chat["tool_choice"]["function"]["name"], "get_current_weather");
}

#[test]
fn test_request_unsupported_tool_types_rejected() {
    for tool_type in ["web_search", "file_search", "computer"] {
        let req = json!({
            "model": "devin/glm-5-2",
            "input": "search online",
            "tools": [{"type": tool_type}]
        });
        let err = responses_to_openai(&req).expect_err("non-function tool must be rejected");
        assert!(err.to_string().contains("only function tools are supported"));
    }
}

// ─── 2. Multi-turn Tool History Validation ───────────────────────────────────

#[test]
fn test_request_valid_multi_turn_tool_history() {
    let req = json!({
        "model": "devin/glm-5-2",
        "input": [
            {"role": "user", "content": "Fetch stock price for AAPL."},
            {
                "type": "function_call",
                "call_id": "call_stock_001",
                "name": "lookup_ticker",
                "arguments": "{\"symbol\":\"AAPL\"}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_stock_001",
                "output": "{\"price\":224.50}"
            }
        ]
    });

    let chat = responses_to_openai(&req).expect("valid multi-turn tool exchange");
    let messages = chat["messages"].as_array().expect("messages");
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "Fetch stock price for AAPL.");

    assert_eq!(messages[1]["role"], "assistant");
    assert_eq!(messages[1]["tool_calls"][0]["id"], "call_stock_001");
    assert_eq!(messages[1]["tool_calls"][0]["function"]["name"], "lookup_ticker");
    assert_eq!(messages[1]["tool_calls"][0]["function"]["arguments"], "{\"symbol\":\"AAPL\"}");

    assert_eq!(messages[2]["role"], "tool");
    assert_eq!(messages[2]["tool_call_id"], "call_stock_001");
    assert_eq!(messages[2]["content"], "{\"price\":224.50}");
}

#[test]
fn test_request_tool_history_is_error_flag_preservation() {
    let req = json!({
        "model": "devin/glm-5-2",
        "input": [
            {
                "type": "function_call",
                "call_id": "call_err_1",
                "name": "execute_cmd",
                "arguments": "{\"cmd\":\"rm -rf /\"}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_err_1",
                "output": "permission denied",
                "is_error": true
            }
        ]
    });

    let chat = responses_to_openai(&req).expect("valid conversion");
    let messages = chat["messages"].as_array().expect("messages");
    let tool_msg = &messages[1];
    assert_eq!(tool_msg["role"], "tool");
    assert_eq!(tool_msg["is_error"], true);
}

#[test]
fn test_request_malformed_tool_arguments_rejected_without_silent_coercion() {
    let req = json!({
        "model": "devin/glm-5-2",
        "input": [
            {
                "type": "function_call",
                "call_id": "call_bad_args",
                "name": "lookup",
                "arguments": "{not_valid_json"
            }
        ]
    });
    let err = responses_to_openai(&req).expect_err("malformed tool arguments must be rejected");
    assert!(err.to_string().contains("malformed JSON in function_call arguments"));
}

#[test]
fn test_request_missing_call_id_or_name_rejected() {
    let missing_name = json!({
        "model": "devin/glm-5-2",
        "input": [{"type": "function_call", "call_id": "call_1", "arguments": "{}"}]
    });
    let err = responses_to_openai(&missing_name).expect_err("missing name rejected");
    assert!(err.to_string().contains("missing function_call name"));

    let missing_call_id = json!({
        "model": "devin/glm-5-2",
        "input": [{"type": "function_call", "name": "lookup", "arguments": "{}"}]
    });
    let err = responses_to_openai(&missing_call_id).expect_err("missing call_id rejected");
    assert!(err.to_string().contains("missing function_call call_id"));

    let missing_out_call_id = json!({
        "model": "devin/glm-5-2",
        "input": [{"type": "function_call_output", "output": "ok"}]
    });
    let err = responses_to_openai(&missing_out_call_id).expect_err("missing output call_id rejected");
    assert!(err.to_string().contains("missing function_call_output call_id"));
}

#[test]
fn test_request_orphan_or_duplicate_tool_calls_rejected() {
    // 1. Orphan output without prior call
    let orphan_output = json!({
        "model": "devin/glm-5-2",
        "input": [
            {"type": "function_call_output", "call_id": "call_ghost", "output": "data"}
        ]
    });
    let err = responses_to_openai(&orphan_output).expect_err("orphan tool output rejected");
    assert!(err.to_string().contains("unmatched function_call_output"));

    // 2. Duplicate tool call IDs
    let duplicate_call = json!({
        "model": "devin/glm-5-2",
        "input": [
            {"type": "function_call", "call_id": "call_dup", "name": "fn1", "arguments": "{}"},
            {"type": "function_call", "call_id": "call_dup", "name": "fn2", "arguments": "{}"}
        ]
    });
    let err = responses_to_openai(&duplicate_call).expect_err("duplicate call ID rejected");
    assert!(err.to_string().contains("duplicate function_call call_id"));
}

// ─── 3. Streaming SSE Lifecycle Tests ────────────────────────────────────────

#[test]
fn test_streaming_lifecycle_text_turn() {
    let mut renderer = ResponsesStreamRenderer::new("devin/glm-5-2".to_string(), 1700000000);

    let mut frames = Vec::new();
    frames.extend(renderer.render(CodexEvent::Created {
        response_id: "resp_devin_text_1".to_string(),
    }));
    frames.extend(renderer.render(CodexEvent::TextDelta("Hello,".to_string())));
    frames.extend(renderer.render(CodexEvent::TextDelta(" world!".to_string())));
    frames.extend(renderer.render(CodexEvent::Completed {
        usage: Some(Usage {
            prompt_tokens: 15,
            completion_tokens: 5,
            total_tokens: 20,
            cached_tokens: 2,
            reasoning_tokens: 0,
            cache_write_tokens: 0,
        }),
    }));

    let parsed = parse_sse_frames(&frames);
    let event_names: Vec<&str> = parsed.iter().map(|(ev, _)| ev.as_str()).collect();

    assert_eq!(
        event_names,
        vec![
            "response.created",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
            "response.output_text.delta",
            "response.output_item.done",
            "response.completed"
        ]
    );

    // Verify response.created payload
    assert_eq!(parsed[0].1["type"], "response.created");
    assert_eq!(parsed[0].1["response"]["id"], "resp_devin_text_1");
    assert_eq!(parsed[0].1["response"]["status"], "in_progress");

    // Verify output_item.added
    assert_eq!(parsed[1].1["type"], "response.output_item.added");
    assert_eq!(parsed[1].1["output_index"], 0);
    assert_eq!(parsed[1].1["item"]["type"], "message");
    assert_eq!(parsed[1].1["item"]["role"], "assistant");

    // Verify content_part.added
    assert_eq!(parsed[2].1["type"], "response.content_part.added");
    assert_eq!(parsed[2].1["output_index"], 0);
    assert_eq!(parsed[2].1["content_index"], 0);
    assert_eq!(parsed[2].1["part"]["type"], "output_text");

    // Verify text deltas
    assert_eq!(parsed[3].1["delta"], "Hello,");
    assert_eq!(parsed[4].1["delta"], " world!");

    // Verify output_item.done
    assert_eq!(parsed[5].1["type"], "response.output_item.done");
    assert_eq!(parsed[5].1["output_index"], 0);
    assert_eq!(parsed[5].1["item"]["status"], "completed");
    assert_eq!(parsed[5].1["item"]["content"][0]["text"], "Hello, world!");

    // Verify response.completed
    assert_eq!(parsed[6].1["type"], "response.completed");
    assert_eq!(parsed[6].1["response"]["status"], "completed");
    assert_eq!(parsed[6].1["response"]["usage"]["input_tokens"], 15);
    assert_eq!(parsed[6].1["response"]["usage"]["output_tokens"], 5);
    assert_eq!(parsed[6].1["response"]["usage"]["total_tokens"], 20);
}

#[test]
fn test_streaming_lifecycle_tool_call_turn() {
    let mut renderer = ResponsesStreamRenderer::new("devin/glm-5-2".to_string(), 1700000000);

    let mut frames = Vec::new();
    frames.extend(renderer.render(CodexEvent::Created {
        response_id: "resp_devin_tool_1".to_string(),
    }));
    frames.extend(renderer.render(CodexEvent::ToolCallBegin {
        output_index: 0,
        call_id: "call_calc_9".to_string(),
        name: "calculate".to_string(),
    }));
    frames.extend(renderer.render(CodexEvent::ToolArgsDelta {
        output_index: 0,
        delta: "{\"expr\":".to_string(),
    }));
    frames.extend(renderer.render(CodexEvent::ToolArgsDelta {
        output_index: 0,
        delta: "\"2+2\"}".to_string(),
    }));
    frames.extend(renderer.render(CodexEvent::Completed {
        usage: Some(Usage {
            prompt_tokens: 30,
            completion_tokens: 10,
            total_tokens: 40,
            cached_tokens: 0,
            reasoning_tokens: 0,
            cache_write_tokens: 0,
        }),
    }));

    let parsed = parse_sse_frames(&frames);
    let event_names: Vec<&str> = parsed.iter().map(|(ev, _)| ev.as_str()).collect();

    assert_eq!(
        event_names,
        vec![
            "response.created",
            "response.output_item.added",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.delta",
            "response.function_call_arguments.done",
            "response.output_item.done",
            "response.completed"
        ]
    );

    // Verify tool item added
    assert_eq!(parsed[1].1["type"], "response.output_item.added");
    assert_eq!(parsed[1].1["output_index"], 0);
    assert_eq!(parsed[1].1["item"]["type"], "function_call");
    assert_eq!(parsed[1].1["item"]["call_id"], "call_calc_9");
    assert_eq!(parsed[1].1["item"]["name"], "calculate");

    // Verify arguments done
    assert_eq!(parsed[4].1["type"], "response.function_call_arguments.done");
    assert_eq!(parsed[4].1["arguments"], "{\"expr\":\"2+2\"}");

    // Verify output_item.done
    assert_eq!(parsed[5].1["type"], "response.output_item.done");
    assert_eq!(parsed[5].1["item"]["status"], "completed");
    assert_eq!(parsed[5].1["item"]["arguments"], "{\"expr\":\"2+2\"}");

    // Verify response.completed
    assert_eq!(parsed[6].1["type"], "response.completed");
    assert_eq!(parsed[6].1["response"]["status"], "completed");
    let output = parsed[6].1["response"]["output"].as_array().expect("output");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0]["type"], "function_call");
    assert_eq!(output[0]["arguments"], "{\"expr\":\"2+2\"}");
}

#[test]
fn test_streaming_reasoning_deltas_lifecycle() {
    let mut renderer = ResponsesStreamRenderer::new("devin/glm-5-2".to_string(), 1700000000);

    let mut frames = Vec::new();
    frames.extend(renderer.render(CodexEvent::Created {
        response_id: "resp_reasoning_1".to_string(),
    }));
    frames.extend(renderer.render(CodexEvent::ReasoningDelta("step 1...".to_string())));
    frames.extend(renderer.render(CodexEvent::ReasoningDelta("step 2...".to_string())));
    frames.extend(renderer.render(CodexEvent::ReasoningSignature("sig-999".to_string())));
    frames.extend(renderer.render(CodexEvent::TextDelta("answer".to_string())));
    frames.extend(renderer.render(CodexEvent::Completed { usage: None }));

    let parsed = parse_sse_frames(&frames);
    let reasoning_deltas: Vec<&Value> = parsed
        .iter()
        .filter(|(ev, _)| ev == "response.reasoning_text.delta")
        .map(|(_, v)| v)
        .collect();

    assert_eq!(reasoning_deltas.len(), 2);
    assert_eq!(reasoning_deltas[0]["delta"], "step 1...");
    assert_eq!(reasoning_deltas[1]["delta"], "step 2...");
}

#[test]
fn test_streaming_interleaved_tools_maintain_stable_ids_and_indices() {
    let mut renderer = ResponsesStreamRenderer::new("devin/glm-5-2".to_string(), 1700000000);

    let mut frames = Vec::new();
    frames.extend(renderer.render(CodexEvent::Created {
        response_id: "resp_interleaved".to_string(),
    }));
    // Text first (output_index 0)
    frames.extend(renderer.render(CodexEvent::TextDelta("Calling tools...".to_string())));

    // Tool 0 begin (output_index should be 1)
    frames.extend(renderer.render(CodexEvent::ToolCallBegin {
        output_index: 0,
        call_id: "call_0".to_string(),
        name: "tool_a".to_string(),
    }));
    // Tool 1 begin (output_index should be 2)
    frames.extend(renderer.render(CodexEvent::ToolCallBegin {
        output_index: 1,
        call_id: "call_1".to_string(),
        name: "tool_b".to_string(),
    }));

    // Tool 0 chunk
    frames.extend(renderer.render(CodexEvent::ToolArgsDelta {
        output_index: 0,
        delta: "{\"a\":1}".to_string(),
    }));
    // Tool 1 chunk
    frames.extend(renderer.render(CodexEvent::ToolArgsDelta {
        output_index: 1,
        delta: "{\"b\":2}".to_string(),
    }));

    frames.extend(renderer.render(CodexEvent::Completed { usage: None }));

    let parsed = parse_sse_frames(&frames);
    let terminal = &parsed.last().unwrap().1["response"];
    let output = terminal["output"].as_array().expect("output array");

    assert_eq!(output.len(), 3, "must have text item and 2 distinct tool items");
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["id"], "msg_0");

    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["id"], "fc_1");
    assert_eq!(output[1]["name"], "tool_a");
    assert_eq!(output[1]["arguments"], "{\"a\":1}");

    assert_eq!(output[2]["type"], "function_call");
    assert_eq!(output[2]["id"], "fc_2");
    assert_eq!(output[2]["name"], "tool_b");
    assert_eq!(output[2]["arguments"], "{\"b\":2}");
}

#[test]
fn test_streaming_output_limit_vs_failure() {
    // 1. Output limit reached -> response.incomplete with reason max_output_tokens
    let mut limit_renderer = ResponsesStreamRenderer::new("devin/glm-5-2".to_string(), 1700000000);
    let mut frames = Vec::new();
    frames.extend(limit_renderer.render(CodexEvent::Created {
        response_id: "resp_limit".to_string(),
    }));
    frames.extend(limit_renderer.render(CodexEvent::TextDelta("truncated text".to_string())));
    frames.extend(limit_renderer.render(CodexEvent::OutputLimitReached));
    frames.extend(limit_renderer.render(CodexEvent::Completed {
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 2048,
            total_tokens: 2058,
            cached_tokens: 0,
            reasoning_tokens: 0,
            cache_write_tokens: 0,
        }),
    }));

    let parsed = parse_sse_frames(&frames);
    let last_event = parsed.last().expect("last event");
    assert_eq!(last_event.0, "response.incomplete");
    assert_eq!(last_event.1["response"]["status"], "incomplete");
    assert_eq!(
        last_event.1["response"]["incomplete_details"]["reason"],
        "max_output_tokens"
    );

    // 2. Failure event -> response.failed
    let mut fail_renderer = ResponsesStreamRenderer::new("devin/glm-5-2".to_string(), 1700000000);
    let fail_frames = fail_renderer.render(CodexEvent::Failed {
        message: "Devin upstream disconnected".to_string(),
    });
    let parsed_fail = parse_sse_frames(&fail_frames);
    assert_eq!(parsed_fail[0].0, "response.failed");
    assert_eq!(parsed_fail[0].1["response"]["status"], "failed");
    assert_eq!(
        parsed_fail[0].1["response"]["error"]["message"],
        "Devin upstream disconnected"
    );
}

#[test]
fn test_streaming_usage_unknown_vs_zero() {
    // 1. Unknown usage (None) must be null, not fabricated zeros
    let mut renderer_unknown =
        ResponsesStreamRenderer::new("devin/glm-5-2".to_string(), 1700000000);
    let frames_unknown = renderer_unknown.render(CodexEvent::Completed { usage: None });
    let parsed_unknown = parse_sse_frames(&frames_unknown);
    let last_unknown = parsed_unknown.last().expect("terminal frame");
    assert_eq!(last_unknown.0, "response.completed");
    let usage_val = &last_unknown.1["response"]["usage"];
    assert!(
        usage_val.is_null(),
        "unknown usage must NOT be fabricated into zeros: {usage_val:?}"
    );

    // 2. Exact zero usage (Some with 0 values)
    let mut renderer_zero =
        ResponsesStreamRenderer::new("devin/glm-5-2".to_string(), 1700000000);
    let frames_zero = renderer_zero.render(CodexEvent::Completed {
        usage: Some(Usage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
            cached_tokens: 0,
            reasoning_tokens: 0,
            cache_write_tokens: 0,
        }),
    });
    let parsed_zero = parse_sse_frames(&frames_zero);
    let last_zero = parsed_zero.last().expect("terminal frame");
    assert_eq!(last_zero.0, "response.completed");
    let usage_zero = &last_zero.1["response"]["usage"];
    assert_eq!(usage_zero["total_tokens"], 0);
    assert_eq!(usage_zero["input_tokens"], 0);
}

#[test]
fn test_responses_error_json_structure_and_into_response() {
    let err = ResponsesError::invalid_request("bad parameter value", Some("test_param"));
    let err_json = err.to_json_value();
    assert_eq!(err_json["error"]["message"], "bad parameter value");
    assert_eq!(err_json["error"]["type"], "invalid_request_error");
    assert_eq!(err_json["error"]["param"], "test_param");

    let renderer = ResponsesStreamRenderer::new("devin/glm-5-2".to_string(), 1700000000);
    let response_val = renderer.into_response();
    assert_eq!(response_val["id"], "resp_1700000000");
    assert_eq!(response_val["status"], "in_progress");
}

// ─── 4. Non-Streaming JSON Response Tests ────────────────────────────────────

#[test]
fn test_nonstream_json_equivalent_to_stream_lifecycle() {
    let events = vec![
        CodexEvent::Created {
            response_id: "resp_nonstream_1".to_string(),
        },
        CodexEvent::ReasoningDelta("pondering".to_string()),
        CodexEvent::ReasoningSignature("sig-xyz".to_string()),
        CodexEvent::TextDelta("Here is the tool call:".to_string()),
        CodexEvent::ToolCallBegin {
            output_index: 0,
            call_id: "call_check_1".to_string(),
            name: "check_health".to_string(),
        },
        CodexEvent::ToolArgsDelta {
            output_index: 0,
            delta: "{\"service\":\"db\"}".to_string(),
        },
        CodexEvent::Completed {
            usage: Some(Usage {
                prompt_tokens: 42,
                completion_tokens: 18,
                total_tokens: 60,
                cached_tokens: 10,
                reasoning_tokens: 5,
                cache_write_tokens: 0,
            }),
        },
    ];

    let nonstream =
        responses_response(&events, "devin/glm-5-2", 1700000000).expect("successful aggregation");

    // Also run through stream renderer
    let mut renderer = ResponsesStreamRenderer::new("devin/glm-5-2".to_string(), 1700000000);
    let mut stream_frames = Vec::new();
    for ev in events {
        stream_frames.extend(renderer.render(ev));
    }
    let parsed_stream = parse_sse_frames(&stream_frames);
    let stream_terminal = &parsed_stream.last().unwrap().1["response"];

    // Nonstream JSON must match stream's terminal response object
    assert_eq!(nonstream["id"], stream_terminal["id"]);
    assert_eq!(nonstream["object"], stream_terminal["object"]);
    assert_eq!(nonstream["status"], stream_terminal["status"]);
    assert_eq!(nonstream["model"], stream_terminal["model"]);
    assert_eq!(nonstream["output"], stream_terminal["output"]);
    assert_eq!(nonstream["usage"], stream_terminal["usage"]);

    // Explicit validation of machine values
    assert_eq!(nonstream["status"], "completed");
    let output = nonstream["output"].as_array().expect("output");
    assert_eq!(output.len(), 2);
    assert_eq!(output[0]["type"], "message");
    assert_eq!(output[0]["content"][0]["text"], "Here is the tool call:");
    assert_eq!(output[1]["type"], "function_call");
    assert_eq!(output[1]["call_id"], "call_check_1");
    assert_eq!(output[1]["name"], "check_health");
    assert_eq!(output[1]["arguments"], "{\"service\":\"db\"}");
}

#[test]
fn test_nonstream_incomplete_and_failure_handling() {
    // 1. Incomplete
    let limit_events = vec![
        CodexEvent::Created {
            response_id: "resp_lim".to_string(),
        },
        CodexEvent::TextDelta("cut off".to_string()),
        CodexEvent::OutputLimitReached,
        CodexEvent::Completed { usage: None },
    ];
    let nonstream_limit =
        responses_response(&limit_events, "devin/glm-5-2", 1700000000).expect("incomplete response");
    assert_eq!(nonstream_limit["status"], "incomplete");
    assert_eq!(
        nonstream_limit["incomplete_details"]["reason"],
        "max_output_tokens"
    );

    // 2. Failure returns Err with failure message
    let fail_events = vec![
        CodexEvent::Created {
            response_id: "resp_fail".to_string(),
        },
        CodexEvent::Failed {
            message: "Connect error resource_exhausted".to_string(),
        },
    ];
    let err = responses_response(&fail_events, "devin/glm-5-2", 1700000000)
        .expect_err("failure must return error");
    assert!(err.to_string().contains("resource_exhausted"));
}

use mahoquot_gateway::compat::claude::{AnthropicDecoder, AnthropicStreamRenderer};
use mahoquot_gateway::compat::events::{CodexEvent, SseParser};
use mahoquot_gateway::compat::gemini::GeminiDecoder;
use mahoquot_gateway::compat::render::{Aggregator, ChunkRenderer, GeminiChunkRenderer};
use serde_json::{json, Value};

fn assert_output_limit(events: Vec<CodexEvent>) {
    let has_tool = events
        .iter()
        .any(|event| matches!(event, CodexEvent::ToolCallBegin { .. }));
    let mut aggregate = Aggregator::new("fixture-model".into(), 0);
    let mut chat = ChunkRenderer::new("fixture-model".into(), 0, true);
    let mut gemini = GeminiChunkRenderer::new("fixture-model".into(), 0);
    let mut anthropic = AnthropicStreamRenderer::new("fixture-model".into(), 0);
    let mut chat_wire = Vec::new();
    let mut gemini_wire = Vec::new();
    let mut anthropic_wire = Vec::new();
    for event in events {
        aggregate.push(event.clone());
        chat_wire.extend(chat.render(event.clone()).into_iter().flatten());
        gemini_wire.extend(gemini.render(event.clone()).into_iter().flatten());
        anthropic_wire.extend(anthropic.render(event).into_iter().flatten());
    }
    assert!(aggregate.failure().is_none());
    let completion = aggregate.into_completion();
    assert_eq!(completion["choices"][0]["finish_reason"], "length");
    assert_eq!(completion["usage"]["total_tokens"], 6);
    if has_tool {
        assert_eq!(
            completion["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":"
        );
    }
    let chat_wire = String::from_utf8(chat_wire).unwrap();
    assert!(
        chat_wire.contains("\"finish_reason\":\"length\""),
        "{chat_wire}"
    );
    assert!(chat_wire.ends_with("data: [DONE]\n\n"));
    let gemini_wire = String::from_utf8(gemini_wire).unwrap();
    assert!(
        gemini_wire.contains("\"finishReason\":\"MAX_TOKENS\""),
        "{gemini_wire}"
    );
    let anthropic_wire = String::from_utf8(anthropic_wire).unwrap();
    assert!(
        anthropic_wire.contains("\"stop_reason\":\"max_tokens\""),
        "{anthropic_wire}"
    );
}

fn anthropic_events(blocks: &[Value]) -> Vec<CodexEvent> {
    let mut decoder = AnthropicDecoder::new();
    let mut events = Vec::new();
    for block in blocks {
        decoder.decode(&serde_json::to_vec(block).unwrap(), &mut events);
    }
    decoder.finish(&mut events);
    events
}

#[test]
fn anthropic_output_limit_survives_protocol_conversion() {
    let events = anthropic_events(&[
        json!({"type":"message_start","message":{"id":"msg-limit","usage":{"input_tokens":5}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}),
        json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ]);
    assert_output_limit(events);
}

#[test]
fn anthropic_json_output_limit_is_length() {
    let result = mahoquot_gateway::compat::claude::anthropic_json_to_openai(
        &json!({
            "content":[{"type":"text","text":"partial"}],
            "stop_reason":"max_tokens",
            "usage":{"input_tokens":5,"output_tokens":1}
        }),
        "fixture-model",
        0,
    );
    assert_eq!(result["choices"][0]["finish_reason"], "length");
    assert_eq!(result["choices"][0]["message"]["content"], "partial");
    assert_eq!(result["usage"]["total_tokens"], 6);
}

#[test]
fn output_limit_takes_precedence_over_partial_tool_calls() {
    let events = anthropic_events(&[
        json!({"type":"message_start","message":{"id":"msg-limit","usage":{"input_tokens":5}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call-1","name":"lookup","input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}),
        json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":1}}),
        json!({"type":"message_stop"}),
    ]);
    assert_output_limit(events);
}

#[test]
fn antigravity_output_limit_survives_protocol_conversion() {
    let mut decoder = GeminiDecoder::new();
    let mut events = Vec::new();
    decoder.decode(&serde_json::to_vec(&json!({
        "response": {
            "candidates": [{"content":{"role":"model","parts":[{"text":"partial"}]},"finishReason":"MAX_TOKENS"}],
            "usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":1,"totalTokenCount":6}
        }
    })).unwrap(), &mut events);
    decoder.finish(&mut events);
    assert_output_limit(events);
}

#[test]
fn codex_output_limit_is_not_an_upstream_failure() {
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    for value in [
        json!({"type":"response.output_text.delta","delta":"partial"}),
        json!({"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":5,"output_tokens":1}}}),
    ] {
        parser.push(format!("data: {value}\n\n").as_bytes(), &mut events);
    }
    parser.finish(&mut events);
    assert_output_limit(events);
}

#[test]
fn other_codex_incomplete_reasons_remain_failures() {
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    parser.push(b"data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"content_filter\"}}}\n\n", &mut events);
    assert!(
        matches!(events.as_slice(), [CodexEvent::Failed { message }] if message == "content_filter")
    );
}

//! P2 wire regression: Devin request builder and strict Connect response decoder.
//! Literal vectors are computed by hand from pinned proto field numbers
//! (proto2 presence), not by re-encoding with the implementation.

use bytes::Bytes;
use prost::Message;
use serde_json::{json, Value};

use mahoquot_gateway::compat::devin::{
    authorization_header, build_chat_request, build_model_configs_request,
    frame_data, frame_end_stream, DevinDecoder, DevinRequestParams,
    MAX_FRAME_SIZE, STREAM_CONTENT_TYPE, UNARY_CONTENT_TYPE,
};
use mahoquot_gateway::compat::devin_proto::{
    ChatMessagePrompt, ChatMessageResponse, GetChatMessageRequest,
};
use mahoquot_gateway::compat::events::CodexEvent;

fn params() -> DevinRequestParams {
    DevinRequestParams {
        token: "tok".into(),
        chat_model_uid: "glm-x".into(),
        supports_vision: false,
        trajectory_id: "T".into(),
        cascade_id: "C".into(),
        execution_id: "E".into(),
        fingerprint: "F".into(),
    }
}

fn counter() -> impl FnMut() -> String {
    let mut n = 0;
    move || {
        n += 1;
        format!("M{n}")
    }
}

fn unhex(hex: &str) -> Vec<u8> {
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
        .collect()
}

// ─── Request builder ────────────────────────────────────────────────────────

/// Independent literal vector: metadata(1) [ide_name=1 chisel,
/// extension_version=2, api_key=3 tok, locale=4 en, os=5 win, ide_version=7,
/// extension_name=12 chisel, f=31 F] + chat_message_prompts(3) [message_id=1,
/// source=2 USER(1), prompt=3 "hi"] + request_type(7)=CASCADE(5) +
/// configuration(8) [num=1, max_tokens=4096, max_newlines=400, temperature 1.0,
/// top_k=40, top_p 0.95] + trajectory_reference(15) [id=1, type=3 CASCADE(4),
/// step=4 USER_INPUT(14)] + cascade_id(16) + planner_mode(20)=DEFAULT(1) +
/// chat_model_uid(21) + execution_id(22).
#[test]
fn minimal_request_matches_independent_wire_vector() {
    let body = json!({"model": "devin/glm-x", "messages": [{"role": "user", "content": "hi"}]});
    let wire = build_chat_request(&body, &params(), &mut counter()).unwrap();
    assert_eq!(
        wire,
        unhex("0a380a0663686973656c1209333030302e322e31371a03746f6b2202656e2a0377696e3a09333030302e322e3137620663686973656cfa0101461a0a0a024d3110011a0268693805421c080110802018900329000000000000f03f382841666666666666ee3f7a070a01541804200e82010143a00101aa0105676c6d2d78b2010145".trim_start()),
        "minimal request wire"
    );
}

#[test]
fn full_history_preserves_roles_tool_reasoning_signature_and_images() {
    let body = json!({
        "messages": [
            {"role": "system", "content": "You are Claude Code, Anthropic's official CLI for Claude"},
            {"role": "user", "content": "earlier"},
            {"role": "user", "content": [
                {"type": "text", "text": "look"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,abc"}}
            ]},
            {"role": "assistant", "content": "", "reasoning_content": "ponder",
             "reasoning_signature": "sig-opaque",
             "tool_calls": [{"id": "call_1", "type": "function",
                             "function": {"name": "inspect", "arguments": "{\"q\":1}"}}]},
            {"role": "tool", "tool_call_id": "call_1", "is_error": true, "content": "boom"},
            {"role": "user", "content": "thanks"}
        ]
    });
    let mut p = params();
    p.supports_vision = true;
    let wire = build_chat_request(&body, &p, &mut counter()).unwrap();
    let req = GetChatMessageRequest::decode(wire.as_slice()).unwrap();
    // The system prompt must survive verbatim (no identity rewriting) into
    // `prompt = 2`; history order and roles user=1 assistant=2 tool=4 preserved.
    assert_eq!(req.prompt.as_deref(), Some("You are Claude Code, Anthropic's official CLI for Claude"));
    let sources: Vec<i32> = req
        .chat_message_prompts
        .iter()
        .map(|m| m.source.unwrap_or(0))
        .collect();
    assert_eq!(sources, vec![1, 1, 2, 4, 1], "user=1 assistant=2 tool=4");
    let history: Vec<&ChatMessagePrompt> = req.chat_message_prompts.iter().collect();
    // Vision-capable: data-url image stays in history, including past turns.
    assert_eq!(history[1].images.len(), 1);
    assert_eq!(history[1].images[0].mime_type.as_deref(), Some("image/png"));
    assert_eq!(history[1].images[0].base64_data.as_deref(), Some("abc"));
    // Assistant turn keeps text, thinking, signature and full tool calls.
    assert_eq!(history[2].thinking.as_deref(), Some("ponder"));
    assert_eq!(history[2].signature.as_deref(), Some("sig-opaque"));
    assert_eq!(history[2].tool_calls.len(), 1);
    assert_eq!(history[2].tool_calls[0].id.as_deref(), Some("call_1"));
    assert_eq!(history[2].tool_calls[0].name.as_deref(), Some("inspect"));
    assert_eq!(history[2].tool_calls[0].arguments_json.as_deref(), Some("{\"q\":1}"));
    // Tool result keeps call id and error flag.
    assert_eq!(history[3].tool_call_id.as_deref(), Some("call_1"));
    assert_eq!(history[3].tool_result_is_error, Some(true));
    assert_eq!(history[3].prompt.as_deref(), Some("boom"));
    // Message ids are caller-injected and distinct.
    let mut ids: Vec<&str> = history.iter().filter_map(|m| m.message_id.as_deref()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), history.len());
}

#[test]
fn schema_descriptions_and_definitions_are_not_stripped() {
    let body = json!({
        "messages": [{"role": "user", "content": "go"}],
        "tools": [{"type": "function", "function": {
            "name": "weather",
            "description": "get weather",
            "parameters": {"type": "object", "properties": {"city": {
                "type": "string", "description": "Korean city", "enum": ["Seoul"]
            }}}
        }}]
    });
    let wire = build_chat_request(&body, &params(), &mut counter()).unwrap();
    let req = GetChatMessageRequest::decode(wire.as_slice()).unwrap();
    assert_eq!(req.tools.len(), 1);
    assert_eq!(req.tools[0].name.as_deref(), Some("weather"));
    assert_eq!(req.tools[0].description.as_deref(), Some("get weather"));
    let schema: Value = serde_json::from_str(req.tools[0].json_schema_string.as_deref().unwrap()).unwrap();
    assert_eq!(schema.pointer("/properties/city/description"), Some(&json!("Korean city")));
    assert_eq!(schema.pointer("/properties/city/enum"), Some(&json!(["Seoul"])));
}

#[test]
fn output_limits_temperature_and_top_p_are_preserved() {
    let body = json!({
        "messages": [{"role": "user", "content": "x"}],
        "max_tokens": 777, "temperature": 0.3, "top_p": 0.9
    });
    let req = GetChatMessageRequest::decode(
        build_chat_request(&body, &params(), &mut counter()).unwrap().as_slice(),
    ).unwrap();
    let cfg = req.configuration.as_ref().unwrap();
    assert_eq!(cfg.max_tokens, Some(777));
    assert_eq!(cfg.temperature, Some(0.3));
    assert_eq!(cfg.top_p, Some(0.9));
    assert_eq!(cfg.num_completions, Some(1));

    // max_completion_tokens counts as the output limit too.
    let body = json!({
        "messages": [{"role": "user", "content": "x"}], "max_completion_tokens": 1234
    });
    let req = GetChatMessageRequest::decode(
        build_chat_request(&body, &params(), &mut counter()).unwrap().as_slice(),
    ).unwrap();
    assert_eq!(req.configuration.unwrap().max_tokens, Some(1234));
}

#[test]
fn absent_options_use_reference_defaults() {
    let body = json!({"messages": [{"role": "user", "content": "x"}]});
    let req = GetChatMessageRequest::decode(
        build_chat_request(&body, &params(), &mut counter()).unwrap().as_slice(),
    ).unwrap();
    let cfg = req.configuration.as_ref().unwrap();
    assert_eq!(cfg.max_tokens, Some(4096));
    assert_eq!(cfg.temperature, Some(1.0));
    assert_eq!(cfg.top_p, Some(0.95));
    assert_eq!(cfg.num_completions, Some(1));
}

#[test]
fn tool_choice_none_removes_tools_but_auto_keeps_them() {
    let tools = json!([{"type": "function", "function": {"name": "f", "description": "d", "parameters": {}}}]);
    let none = json!({"messages": [{"role": "user", "content": "x"}], "tools": tools, "tool_choice": "none"});
    let req = GetChatMessageRequest::decode(
        build_chat_request(&none, &params(), &mut counter()).unwrap().as_slice(),
    ).unwrap();
    assert!(req.tools.is_empty(), "tool_choice none must drop tools");

    let auto = json!({"messages": [{"role": "user", "content": "x"}], "tools": tools, "tool_choice": "auto"});
    let req = GetChatMessageRequest::decode(
        build_chat_request(&auto, &params(), &mut counter()).unwrap().as_slice(),
    ).unwrap();
    assert_eq!(req.tools.len(), 1);
}

#[test]
fn forced_tool_choice_n_1_strict_output_are_rejected() {
    let tools = json!([{"type": "function", "function": {"name": "f", "description": "d", "parameters": {}}}]);
    let base = json!({"messages": [{"role": "user", "content": "x"}], "tools": tools});
    let forced = json!({"tool_choice": {"type": "function", "function": {"name": "f"}}});
    let required = json!({"tool_choice": "required"});
    let named = json!({"tool_choice": "f"});
    for choice in [forced, required, named] {
        let body = merge(&base, &choice);
        assert!(
            build_chat_request(&body, &params(), &mut counter()).is_err(),
            "forced tool choice must be rejected: {choice}"
        );
    }
    let n2 = merge(&base, &json!({"n": 2}));
    assert!(build_chat_request(&n2, &params(), &mut counter()).is_err(), "n>1 must be rejected");
    let strict = json!({"messages": [{"role": "user", "content": "x"}], "tools": [{
        "type": "function", "function": {"name": "f", "description": "d", "parameters": {}, "strict": true}
    }]});
    assert!(build_chat_request(&strict, &params(), &mut counter()).is_err(), "strict output must be rejected");
}

fn merge(base: &Value, patch: &Value) -> Value {
    let mut out = base.clone();
    let obj = out.as_object_mut().unwrap();
    for (k, v) in patch.as_object().unwrap() {
        obj.insert(k.clone(), v.clone());
    }
    out
}

#[test]
fn remote_images_and_non_vision_models_are_rejected_upstream_of_wire() {
    let remote = json!({"supports_vision": true, "messages": [{"role": "user", "content": [
        {"type": "image_url", "image_url": {"url": "https://cdn.example/i.png"}}]}]});
    let mut p = params();
    p.supports_vision = true;
    assert!(build_chat_request(&remote, &p, &mut counter()).is_err(), "remote image url");

    let vision_body = json!({"messages": [{"role": "user", "content": [
        {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,zz"}}]}]});
    assert!(build_chat_request(&vision_body, &params(), &mut counter()).is_err(), "non-vision model with image");

    p.supports_vision = false;
    let plain = json!({"messages": [{"role": "user", "content": "no images here"}]});
    assert!(build_chat_request(&plain, &p, &mut counter()).is_ok(), "text without vision");
}

#[test]
fn tool_result_without_call_id_is_rejected() {
    let body = json!({"messages": [
        {"role": "user", "content": "x"},
        {"role": "tool", "content": "boom"}
    ]});
    assert!(build_chat_request(&body, &params(), &mut counter()).is_err(), "missing tool_call_id");
}

#[test]
fn metadata_keeps_api_key_and_literal_header_contract() {
    let body = json!({"messages": [{"role": "user", "content": "hi"}]});
    let wire = build_chat_request(&body, &params(), &mut counter()).unwrap();
    let req = GetChatMessageRequest::decode(wire.as_slice()).unwrap();
    assert_eq!(req.metadata.unwrap().api_key.as_deref(), Some("tok"));
    // The account layer's header contract is literal, not base64 Basic auth.
    assert_eq!(authorization_header("abc"), "Basic abc-abc");
    assert_eq!(STREAM_CONTENT_TYPE, "application/connect+proto");
    assert_eq!(UNARY_CONTENT_TYPE, "application/proto");
    assert_eq!(MAX_FRAME_SIZE, 16 * 1024 * 1024);
}

/// Unary GetCascadeModelConfigs body is framing-free protobuf (vector C).
#[test]
fn unary_model_request_has_no_connect_envelope() {
    let wire = build_model_configs_request("tok");
    assert_eq!(
        wire,
        unhex("0a340a0663686973656c1209333030302e322e31371a03746f6b2202656e2a0377696e3a09333030302e322e3137620663686973656c"),
        "unary request wire"
    );
    assert_eq!(wire[0], 0x0a, "framing-free protobuf starts with the metadata tag");
}

// ─── Pinned response schema (field numbers) ────────────────────────────────

/// Independent literal vector for GetChatMessageResponse: message_id=1 "r1",
/// delta_text=3 "a", stop_reason=5 FUNCTION_CALL(10), delta_tool_calls=6
/// [id=1 c1, name=2 f, arguments_json=3 "{}"], usage=7 [input=2 10,
/// output=3 20, cache_write=4 6, cache_read=5 5, model_uid=9 glm-x],
/// delta_thinking=9 "t", delta_signature=10 "s", thinking_redacted=11 true,
/// actual_model_uid=23 glm-x, unknown field 50 varint 1.
#[test]
fn response_encoding_matches_pinned_field_numbers() {
    use mahoquot_gateway::compat::devin_proto::{ChatToolCall, ModelUsageStats};
    use prost::Message;

    let msg = ChatMessageResponse {
        message_id: Some("r1".into()),
        timestamp: None,
        delta_text: Some("a".into()),
        stop_reason: Some(10),
        delta_tool_calls: vec![ChatToolCall {
            id: Some("c1".into()),
            name: Some("f".into()),
            arguments_json: Some("{}".into()),
        }],
        usage: Some(ModelUsageStats {
            input_tokens: Some(10),
            output_tokens: Some(20),
            cache_write_tokens: Some(6),
            cache_read_tokens: Some(5),
            model_uid: Some("glm-x".into()),
        }),
        delta_thinking: Some("t".into()),
        delta_signature: Some("s".into()),
        thinking_redacted: Some(true),
        actual_model_uid: Some("glm-x".into()),
    };
    let mut wire = Vec::new();
    msg.encode(&mut wire).unwrap();
    let mut expected = unhex("0a0272311a0161280a320b0a0263311201661a027b7d3a0f100a1814200628054a05676c6d2d784a01745201735801ba0105676c6d2d78");
    // Unknown fields must be tolerated on the wire; the implementation must
    // not reject payloads carrying them.
    wire.extend(unhex("900301"));
    expected.extend(unhex("900301"));
    assert_eq!(wire, expected, "response schema field numbers");
    let decoded = ChatMessageResponse::decode(expected.as_slice()).unwrap();
    assert_eq!(decoded.actual_model_uid.as_deref(), Some("glm-x"));
    assert_eq!(decoded.stop_reason, Some(10));
}

// ─── Connect response decoder ───────────────────────────────────────────────

fn rich_payload() -> Vec<u8> {
    use mahoquot_gateway::compat::devin_proto::{ChatToolCall, ModelUsageStats};
    use prost::Message;
    let mut payload = Vec::new();
    ChatMessageResponse {
        message_id: Some("r1".into()),
        timestamp: None,
        delta_text: Some("a".into()),
        stop_reason: Some(10),
        delta_tool_calls: vec![ChatToolCall {
            id: Some("c1".into()),
            name: Some("f".into()),
            arguments_json: Some("{}".into()),
        }],
        usage: Some(ModelUsageStats {
            input_tokens: Some(10),
            output_tokens: Some(20),
            cache_write_tokens: Some(6),
            cache_read_tokens: Some(5),
            model_uid: Some("glm-x".into()),
        }),
        delta_thinking: Some("t".into()),
        delta_signature: Some("s".into()),
        thinking_redacted: Some(true),
        actual_model_uid: Some("glm-x".into()),
    }
    .encode(&mut payload)
    .unwrap();
    frame_data(&payload)
}

fn rich_stream() -> Vec<u8> {
    let mut wire = rich_payload();
    wire.extend(frame_end_stream("{\"metadata\":{}}"));
    wire
}

fn decode_all(wire: &[u8]) -> (Vec<CodexEvent>, DevinDecoder) {
    let mut decoder = DevinDecoder::new();
    let mut events = Vec::new();
    decoder.decode(wire, &mut events);
    decoder.finish(&mut events);
    (events, decoder)
}

#[test]
fn stream_decodes_thinking_signature_redaction_text_tool_and_usage() {
    let (events, decoder) = decode_all(&rich_stream());
    assert_eq!(
        events,
        vec![
            CodexEvent::ReasoningDelta("t".into()),
            CodexEvent::ReasoningRedacted,
            CodexEvent::ReasoningSignature("s".into()),
            CodexEvent::TextDelta("a".into()),
            CodexEvent::ToolCallBegin {
                output_index: 0,
                call_id: "c1".into(),
                name: "f".into(),
            },
            CodexEvent::ToolArgsDelta {
                output_index: 0,
                delta: "{}".into(),
            },
            CodexEvent::Completed {
                usage: Some(mahoquot_gateway::compat::events::Usage {
                    prompt_tokens: 10,
                    completion_tokens: 20,
                    total_tokens: 30,
                    cached_tokens: 5,
                    reasoning_tokens: 0,
                    cache_write_tokens: 6,
                }),
            },
        ],
        "thinking/signature/redaction/text/tool order"
    );
    let outcome = decoder.outcome();
    assert_eq!(outcome.actual_model_uid.as_deref(), Some("glm-x"));
    assert!(outcome.terminated, "valid EndStream seen");
    assert!(outcome.error_code.is_none());
}

#[test]
fn stop_frame_alone_does_not_emit_success_until_valid_endstream() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::ChatMessageResponse;
    let mut payload = Vec::new();
    ChatMessageResponse { stop_reason: Some(10), ..Default::default() }
        .encode(&mut payload)
        .unwrap();
    let mut wire = frame_data(&payload);
    // No EndStream frame: EOF after a stop frame is a truncated stream.
    let (events, decoder) = decode_all(&wire);
    assert!(events.iter().any(|e| matches!(e, CodexEvent::Failed { .. })), "{events:?}");
    assert!(events.iter().all(|e| !matches!(e, CodexEvent::Completed { .. })), "{events:?}");
    assert!(!decoder.outcome().terminated);

    // Same wire with the EndStream appended does complete.
    wire.extend(frame_end_stream("{\"metadata\":{}}"));
    let (events, _) = decode_all(&wire);
    assert!(events.iter().any(|e| matches!(e, CodexEvent::Completed { .. })), "{events:?}");
    assert!(events.iter().all(|e| !matches!(e, CodexEvent::Failed { .. })), "{events:?}");
}

#[test]
fn code_only_terminal_error_fails_with_code_preserved() {
    let wire = [rich_payload(), frame_end_stream("{\"error\":{\"code\":\"unauthenticated\"}}")].concat();
    let (events, decoder) = decode_all(&wire);
    let failed: Vec<&String> = events.iter().filter_map(|e| match e {
        CodexEvent::Failed { message } => Some(message),
        _ => None,
    }).collect();
    assert_eq!(failed.len(), 1, "{events:?}");
    assert!(failed[0].contains("unauthenticated"), "code-only error message");
    assert_eq!(decoder.outcome().error_code.as_deref(), Some("unauthenticated"));
    // A later failure never emits normal completion.
    assert!(events.iter().all(|e| !matches!(e, CodexEvent::Completed { .. })), "{events:?}");
}

#[test]
fn duplicate_terminal_and_trailing_data_are_rejected() {
    let clean = rich_stream();
    let duplicate = [clean.clone(), frame_end_stream("{\"metadata\":{}}")].concat();
    let (events, _) = decode_all(&duplicate);
    assert!(events.iter().any(|e| matches!(e, CodexEvent::Failed { .. })), "duplicate terminal: {events:?}");

    let trailing = [clean, frame_data(b"\x00")].concat();
    let (events, _) = decode_all(&trailing);
    assert!(events.iter().any(|e| matches!(e, CodexEvent::Failed { .. })), "data after terminal: {events:?}");
}

#[test]
fn unnegotiated_compression_flags_are_rejected() {
    for flags in [1u8, 3] {
        let mut frame = vec![flags];
        frame.extend_from_slice(&2u32.to_be_bytes());
        frame.extend_from_slice(b"{}");
        let (events, _) = decode_all(&frame);
        assert!(
            events.iter().any(|e| matches!(e, CodexEvent::Failed { message } if message.contains("compression"))),
            "flag {flags}: {events:?}"
        );
    }
}

#[test]
fn oversize_frame_is_rejected_without_buffering() {
    let mut frame = vec![0u8];
    frame.extend_from_slice(&((MAX_FRAME_SIZE + 1) as u32).to_be_bytes());
    let (events, _) = decode_all(&frame);
    assert!(events.iter().any(|e| matches!(e, CodexEvent::Failed { .. })), "oversize: {events:?}");
    // Exactly the limit is still accepted structurally (no rejection on size).
    let mut limit_frame = vec![0u8];
    limit_frame.extend_from_slice(&(MAX_FRAME_SIZE as u32).to_be_bytes());
    let mut decoder = DevinDecoder::new();
    let mut events = Vec::new();
    decoder.decode(&limit_frame, &mut events);
    assert!(events.is_empty(), "payload not yet arrived, must not reject: {events:?}");
}

#[test]
fn truncated_frame_fails_at_eof() {
    let wire = rich_stream();
    let (events, _) = decode_all(&wire[..wire.len() - 4]);
    assert!(events.iter().any(|e| matches!(e, CodexEvent::Failed { .. })), "truncated: {events:?}");
    assert!(events.iter().all(|e| !matches!(e, CodexEvent::Completed { .. })), "{events:?}");
}

#[test]
fn malformed_proto_and_malformed_endstream_json_fail() {
    let bad_proto = [frame_data(b"\xff\xff\xff\x07"), frame_end_stream("{\"metadata\":{}}")].concat();
    let (events, _) = decode_all(&bad_proto);
    assert!(events.iter().any(|e| matches!(e, CodexEvent::Failed { message } if message.contains("protobuf"))), "{events:?}");

    let bad_json = [frame_data(b"1a0161"), frame_end_stream("{nope")].concat();
    let (events, _) = decode_all(&bad_json);
    assert!(events.iter().any(|e| matches!(e, CodexEvent::Failed { .. })), "malformed endstream: {events:?}");
}

#[test]
fn korean_text_survives_every_chunk_split() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::ChatMessageResponse;
    let mut payload = Vec::new();
    ChatMessageResponse { delta_text: Some("안녕하세요".into()), ..Default::default() }
        .encode(&mut payload)
        .unwrap();
    let mut wire = frame_data(&payload);
    wire.extend(frame_end_stream("{\"metadata\":{}}"));
    for split in 0..=wire.len() {
        let mut decoder = DevinDecoder::new();
        let mut events = Vec::new();
        decoder.decode(&wire[..split], &mut events);
        decoder.decode(&wire[split..], &mut events);
        decoder.finish(&mut events);
        let text: String = events.iter().filter_map(|e| match e {
            CodexEvent::TextDelta(t) => Some(t.clone()),
            _ => None,
        }).collect();
        assert_eq!(text, "안녕하세요", "split {split} corrupted the Korean text");
        assert!(events.iter().all(|e| !matches!(e, CodexEvent::Failed { .. })), "split {split}: {events:?}");
    }
}

#[test]
fn every_chunk_split_produces_identical_events() {
    let wire = rich_stream();
    let (expected, _) = decode_all(&wire);
    for split in 0..=wire.len() {
        let mut decoder = DevinDecoder::new();
        let mut events = Vec::new();
        decoder.decode(&wire[..split], &mut events);
        decoder.decode(&wire[split..], &mut events);
        decoder.finish(&mut events);
        assert_eq!(events, expected, "split {split}");
    }
    // Many frames coalesced into one byte blob also parse.
    let multi = [rich_payload(), rich_stream()].concat();
    let (events, _) = decode_all(&multi);
    assert!(events.iter().any(|e| matches!(e, CodexEvent::Completed { .. })), "coalesced frames");
    assert!(events.iter().all(|e| !matches!(e, CodexEvent::Failed { .. })), "coalesced frames must not fail");
}

#[test]
fn missing_endstream_at_every_frame_boundary_fails() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::ChatMessageResponse;
    let mut payload = Vec::new();
    ChatMessageResponse { delta_text: Some("a".into()), ..Default::default() }
        .encode(&mut payload)
        .unwrap();
    let wire = frame_data(&payload);
    for split in 0..wire.len() {
        let mut decoder = DevinDecoder::new();
        let mut events = Vec::new();
        decoder.decode(&wire[..split], &mut events);
        decoder.finish(&mut events);
        assert!(
            events.iter().any(|e| matches!(e, CodexEvent::Failed { .. })),
            "partial frame at {split} must fail, not complete: {events:?}"
        );
    }
}

#[test]
fn usage_is_none_when_absent_and_final_snapshot_wins_without_double_count() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::{ChatMessageResponse, ModelUsageStats};
    let snapshot = |input: u64, output: u64, read: u64| ChatMessageResponse {
        usage: Some(ModelUsageStats {
            input_tokens: Some(input),
            output_tokens: Some(output),
            cache_read_tokens: Some(read),
            ..Default::default()
        }),
        ..Default::default()
    };
    let mut wire = Vec::new();
    let mut buf = Vec::new();
    snapshot(10, 20, 5).encode(&mut buf).unwrap();
    wire.extend(frame_data(&buf));
    let mut buf = Vec::new();
    snapshot(30, 40, 7).encode(&mut buf).unwrap();
    wire.extend(frame_data(&buf));
    wire.extend(frame_end_stream("{\"metadata\":{}}"));

    let (events, _) = decode_all(&wire);
    let completed: Vec<_> = events.iter().filter_map(|e| match e {
        CodexEvent::Completed { usage } => usage.clone(),
        _ => None,
    }).collect();
    assert_eq!(completed.len(), 1, "single terminal usage");
    let usage = &completed[0];
    // Last snapshot wins, never summed: 30/40 not 40/60; total excludes cache.
    assert_eq!(usage.prompt_tokens, 30);
    assert_eq!(usage.completion_tokens, 40);
    assert_eq!(usage.total_tokens, 70);
    assert_eq!(usage.cached_tokens, 7);
    assert_eq!(usage.cache_write_tokens, 0);

    // A stream with no usage frame completes with None, not a fake zero.
    let mut plain = Vec::new();
    let mut buf = Vec::new();
    ChatMessageResponse { delta_text: Some("x".into()), ..Default::default() }
        .encode(&mut buf)
        .unwrap();
    plain.extend(frame_data(&buf));
    plain.extend(frame_end_stream("{\"metadata\":{}}"));
    let (events, _) = decode_all(&plain);
    let completed: Vec<_> = events.iter().filter_map(|e| match e {
        CodexEvent::Completed { usage } => Some(usage.clone()),
        _ => None,
    }).collect();
    assert_eq!(completed.len(), 1);
    assert!(completed[0].is_none(), "usage absent stays None: {completed:?}");
}

#[test]
fn ambiguous_idless_tool_delta_fails_but_single_tool_attaches() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::{ChatMessageResponse, ChatToolCall};
    let delta = |id: Option<&str>, name: Option<&str>, args: &str| ChatMessageResponse {
        delta_tool_calls: vec![ChatToolCall {
            id: id.map(String::from),
            name: name.map(String::from),
            arguments_json: Some(args.into()),
        }],
        ..Default::default()
    };
    // Two identified tools, then an ID-less delta: ambiguous, protocol error.
    let mut wire = Vec::new();
    for d in [
        delta(Some("c1"), Some("f1"), "{\"a\":"),
        delta(Some("c2"), Some("f2"), "{\"b\":"),
        delta(None, None, "1}"),
    ] {
        let mut buf = Vec::new();
        d.encode(&mut buf).unwrap();
        wire.extend(frame_data(&buf));
    }
    wire.extend(frame_end_stream("{\"metadata\":{}}"));
    let (events, _) = decode_all(&wire);
    assert!(
        events.iter().any(|e| matches!(e, CodexEvent::Failed { message } if message.contains("ambiguous"))),
        "ambiguous id-less delta: {events:?}"
    );

    // One tool only: ID-less delta attaches to the active call.
    let mut wire = Vec::new();
    for d in [delta(Some("c1"), Some("f1"), "{\"a\":"), delta(None, None, "1}")] {
        let mut buf = Vec::new();
        d.encode(&mut buf).unwrap();
        wire.extend(frame_data(&buf));
    }
    wire.extend(frame_end_stream("{\"metadata\":{}}"));
    let (events, _) = decode_all(&wire);
    let args: String = events.iter().filter_map(|e| match e {
        CodexEvent::ToolArgsDelta { delta, .. } => Some(delta.clone()),
        _ => None,
    }).collect();
    assert_eq!(args, "{\"a\":1}");
    assert!(events.iter().all(|e| !matches!(e, CodexEvent::Failed { .. })), "{events:?}");
}

#[test]
fn tool_begin_is_delayed_until_name_is_known() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::{ChatMessageResponse, ChatToolCall};
    let with = |id: Option<&str>, name: Option<&str>, args: Option<&str>| ChatMessageResponse {
        delta_tool_calls: vec![ChatToolCall {
            id: id.map(String::from),
            name: name.map(String::from),
            arguments_json: args.map(String::from),
        }],
        ..Default::default()
    };
    let mut wire = Vec::new();
    for d in [
        with(Some("c1"), Some("weather"), Some("{\"city\":")),
        with(None, None, Some("\"seoul\"}")),
    ] {
        let mut buf = Vec::new();
        d.encode(&mut buf).unwrap();
        wire.extend(frame_data(&buf));
    }
    wire.extend(frame_end_stream("{\"metadata\":{}}"));
    let (events, _) = decode_all(&wire);
    // The begin carries the name and stable interleaved index 0; the id-less
    // continuation attached to the single active call, with no second begin.
    let begins: Vec<_> = events.iter().filter_map(|e| match e {
        CodexEvent::ToolCallBegin { output_index, call_id, name } => Some((*output_index, call_id.clone(), name.clone())),
        _ => None,
    }).collect();
    assert_eq!(begins, vec![(0, "c1".to_string(), "weather".to_string())], "{events:?}");
    let args: String = events.iter().filter_map(|e| match e {
        CodexEvent::ToolArgsDelta { delta, .. } => Some(delta.clone()),
        _ => None,
    }).collect();
    assert_eq!(args, "{\"city\":\"seoul\"}");
    assert!(events.iter().all(|e| !matches!(e, CodexEvent::Failed { .. })), "{events:?}");
}

#[test]
fn malformed_final_arguments_are_never_repaired_to_empty_object() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::{ChatMessageResponse, ChatToolCall};
    let mut wire = Vec::new();
    {
        let d = ChatMessageResponse {
        stop_reason: Some(10),
        delta_tool_calls: vec![ChatToolCall {
            id: Some("c1".into()),
            name: Some("f".into()),
            arguments_json: Some("{oops".into()),
        }],
        ..Default::default()
    };
        let mut buf = Vec::new();
        d.encode(&mut buf).unwrap();
        wire.extend(frame_data(&buf));
    }
    wire.extend(frame_end_stream("{\"metadata\":{}}"));
    let (events, _) = decode_all(&wire);
    let args: String = events.iter().filter_map(|e| match e {
        CodexEvent::ToolArgsDelta { delta, .. } => Some(delta.clone()),
        _ => None,
    }).collect();
    assert_eq!(args, "{oops", "malformed arguments preserved verbatim");
    assert!(events.iter().all(|e| !matches!(e, CodexEvent::Failed { .. })), "{events:?}");
}

#[test]
fn interleaved_tool_indices_are_stable_across_text_and_reasoning() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::{ChatMessageResponse, ChatToolCall};
    let mut wire = Vec::new();
    for d in [
        ChatMessageResponse { delta_text: Some("a".into()), ..Default::default() },
        ChatMessageResponse { delta_tool_calls: vec![ChatToolCall {
            id: Some("c1".into()), name: Some("one".into()), arguments_json: Some("{\"x\":".into()),
        }], ..Default::default() },
        ChatMessageResponse { delta_thinking: Some("think".into()), ..Default::default() },
        ChatMessageResponse { delta_tool_calls: vec![ChatToolCall {
            id: Some("c2".into()), name: Some("two".into()), arguments_json: Some("{\"y\":1}".into()),
        }], ..Default::default() },
    ] {
        let mut buf = Vec::new();
        d.encode(&mut buf).unwrap();
        wire.extend(frame_data(&buf));
    }
    wire.extend(frame_end_stream("{\"metadata\":{}}"));
    let (events, _) = decode_all(&wire);
    let indices: Vec<u64> = events.iter().filter_map(|e| match e {
        CodexEvent::ToolCallBegin { output_index, .. } => Some(*output_index),
        _ => None,
    }).collect();
    assert_eq!(indices, vec![0, 1], "stable interleaved indices: {events:?}");
    let delta_for = |index: u64| events.iter().filter_map(move |e| match e {
        CodexEvent::ToolArgsDelta { output_index, delta } if *output_index == index => Some(delta.clone()),
        _ => None,
    }).collect::<Vec<_>>();
    assert_eq!(delta_for(0), vec!["{\"x\":"]);
    assert_eq!(delta_for(1), vec!["{\"y\":1}"]);
}

#[test]
fn output_limit_stop_reasons_surface_output_limit_reached() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::ChatMessageResponse;
    for stop in [3u64, 1, 9] {
        let mut wire = Vec::new();
        let mut buf = Vec::new();
        ChatMessageResponse { stop_reason: Some(stop as i32), ..Default::default() }
            .encode(&mut buf)
            .unwrap();
        wire.extend(frame_data(&buf));
        wire.extend(frame_end_stream("{\"metadata\":{}}"));
        let (events, _) = decode_all(&wire);
        assert!(
            events.iter().any(|e| matches!(e, CodexEvent::OutputLimitReached)),
            "stop {stop}: {events:?}"
        );
        assert!(events.iter().any(|e| matches!(e, CodexEvent::Completed { .. })), "stop {stop}: {events:?}");
    }
}

#[test]
fn outcome_reports_stop_reason_and_error_metadata() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::ChatMessageResponse;
    let mut wire = Vec::new();
    let mut buf = Vec::new();
    ChatMessageResponse { stop_reason: Some(10), actual_model_uid: Some("glm-x".into()), ..Default::default() }
        .encode(&mut buf)
        .unwrap();
    wire.extend(frame_data(&buf));
    wire.extend(frame_end_stream("{\"metadata\":{}}"));
    let (_, decoder) = decode_all(&wire);
    let outcome = decoder.outcome();
    assert_eq!(outcome.stop_reason, Some(10));
    assert_eq!(outcome.actual_model_uid.as_deref(), Some("glm-x"));
    assert!(outcome.terminated);
    assert!(outcome.error_code.is_none());
    assert!(outcome.error_message.is_none());
}

// ─── Compat integration ─────────────────────────────────────────────────────

#[test]
fn devin_stream_aggregates_into_chat_completion_with_tools() {
    use mahoquot_gateway::compat::devin_proto::{ChatMessageResponse, ChatToolCall};
    use mahoquot_gateway::compat::Protocol;
    use prost::Message;
    let mut wire = Vec::new();
    for d in [
        ChatMessageResponse { delta_text: Some("hello ".into()), ..Default::default() },
        ChatMessageResponse { delta_tool_calls: vec![ChatToolCall {
            id: Some("c1".into()), name: Some("weather".into()), arguments_json: Some("{\"city\":\"Seoul\"}".into()),
        }], ..Default::default() },
    ] {
        let mut buf = Vec::new();
        d.encode(&mut buf).unwrap();
        wire.extend(frame_data(&buf));
    }
    wire.extend(frame_end_stream("{\"metadata\":{}}"));

    let payload = mahoquot_gateway::compat::aggregate(
        &wire, "devin/glm-x".into(), 0, Protocol::Devin,
        mahoquot_gateway::compat::ReplyShape::Chat,
    ).unwrap();
    let message = &payload["choices"][0]["message"];
    assert_eq!(message["content"], "hello ");
    assert_eq!(message["tool_calls"][0]["id"], "c1");
    assert_eq!(message["tool_calls"][0]["function"]["name"], "weather");
    assert_eq!(message["tool_calls"][0]["function"]["arguments"], "{\"city\":\"Seoul\"}");
    assert_eq!(payload["choices"][0]["finish_reason"], "tool_calls");
}

#[test]
fn devin_binary_stream_passes_open_stream_protocol_gate() {
    use mahoquot_gateway::compat::Protocol;
    let binary = frame_data(b"\x1a\x01a");
    assert!(
        !mahoquot_gateway::compat::looks_like_sse(&binary),
        "connect frames are binary, not sse"
    );
    assert_ne!(Protocol::Devin, Protocol::Cursor, "Devin is its own protocol");
    let _ = Bytes::new();
}

// ─── Defect regression tests ────────────────────────────────────────────────

#[test]
fn defect_1_debug_and_errors_redact_secrets_and_credentials() {
    // (1a) DevinRequestParams Debug redacts the session token
    let mut p = params();
    p.token = "super_secret_session_token_12345".into();
    let debug_repr = format!("{p:?}");
    assert!(
        !debug_repr.contains("super_secret_session_token_12345"),
        "token must not leak in DevinRequestParams Debug: {debug_repr}"
    );
    assert!(debug_repr.contains("[REDACTED]"), "Debug output should contain [REDACTED]");

    // (1b) Remote image URLs with user credentials, query parameters, path or fragment redact secrets
    p.supports_vision = true;
    let bad_remote_body = json!({
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image_url",
                "image_url": {"url": "https://admin:my_secret_password@images.example.com/secret_path_123/asset.png?token=secret_query_token_abc#secret_fragment_456"}
            }]
        }]
    });
    let err = build_chat_request(&bad_remote_body, &p, &mut counter()).unwrap_err();
    assert!(
        !err.contains("my_secret_password"),
        "URL userinfo must not leak in error message: {err}"
    );
    assert!(
        !err.contains("secret_query_token_abc"),
        "URL query tokens must not leak in error message: {err}"
    );
    assert!(
        !err.contains("secret_path_123"),
        "URL path must not leak in error message: {err}"
    );
    assert!(
        !err.contains("secret_fragment_456"),
        "URL fragment must not leak in error message: {err}"
    );
    assert!(
        err.contains("remote image url is unsupported"),
        "error should reject remote image URL meaningfully: {err}"
    );

    // (1c) Upstream error.message with arbitrary bare token is not reflected; recognized code preserved
    let err_endstream = frame_end_stream(
        r#"{"error":{"code":"unauthenticated","message":"invalid token secret_ABC_999 with arbitrary bare token and leak https://user:leak_pass@auth.example.com/check?api_key=leak_key_99"}}"#
    );
    let (events, decoder) = decode_all(&err_endstream);
    let outcome_msg = decoder.outcome().error_message.as_deref().unwrap_or_default();
    assert!(
        !outcome_msg.contains("secret_ABC_999"),
        "Bare token must not be reflected in outcome error message: {outcome_msg}"
    );
    assert!(
        !outcome_msg.contains("leak_pass"),
        "URL password must not be reflected in outcome error message: {outcome_msg}"
    );
    assert!(
        !outcome_msg.contains("leak_key_99"),
        "API key query must not be reflected in outcome error message: {outcome_msg}"
    );
    assert_eq!(
        decoder.outcome().error_code.as_deref(),
        Some("unauthenticated"),
        "recognized error code must be preserved for relay feedback"
    );
    let failed_event_msg = events.iter().find_map(|e| match e {
        CodexEvent::Failed { message } => Some(message.clone()),
        _ => None,
    }).unwrap_or_default();
    assert!(
        !failed_event_msg.contains("secret_ABC_999"),
        "Bare token must not be reflected in failed event message: {failed_event_msg}"
    );

    // (1d) Upstream error with unknown code is normalized to finite safe code "unknown"
    let unknown_code_endstream = frame_end_stream(
        r#"{"error":{"code":"secret_code_custom_leak_123","message":"sensitive upstream details with secret_xyz"}}"#
    );
    let (events2, decoder2) = decode_all(&unknown_code_endstream);
    assert_eq!(
        decoder2.outcome().error_code.as_deref(),
        Some("unknown"),
        "unknown error code must normalize to safe 'unknown' code"
    );
    let outcome_msg2 = decoder2.outcome().error_message.as_deref().unwrap_or_default();
    assert!(
        !outcome_msg2.contains("secret_code_custom_leak_123"),
        "Custom code must not leak: {outcome_msg2}"
    );
    assert!(
        !outcome_msg2.contains("secret_xyz"),
        "Custom message must not leak: {outcome_msg2}"
    );
    let failed_msg2 = events2.iter().find_map(|e| match e {
        CodexEvent::Failed { message } => Some(message.clone()),
        _ => None,
    }).unwrap_or_default();
    assert!(
        !failed_msg2.contains("secret_code_custom_leak_123"),
        "Custom code must not leak in failed event: {failed_msg2}"
    );
}

#[test]
fn defect_2_oversized_frame_header_rejected_before_allocating_payload_or_appending_chunk() {
    let mut decoder = DevinDecoder::new();
    let mut events = Vec::new();
    // 5-byte header indicating 20 MiB frame (> MAX_FRAME_SIZE = 16 MiB)
    let len_20mib: u32 = 20 * 1024 * 1024;
    let mut chunk = vec![0u8];
    chunk.extend_from_slice(&len_20mib.to_be_bytes());
    // Large same-chunk payload: 512 KiB
    chunk.resize(5 + 512 * 1024, 0xAA);
    decoder.decode(&chunk, &mut events);
    assert!(
        events.iter().any(|e| matches!(e, CodexEvent::Failed { message } if message.contains("too large"))),
        "oversized header must fail immediately without allocating payload: {events:?}"
    );
}

#[test]
fn defect_3_terminated_stream_rejects_subsequent_chunks_and_trailing_data_never_emits_completed() {
    // (3a) Same-chunk terminal followed by junk must NEVER emit Completed before Failed
    let mut wire = frame_end_stream(r#"{"metadata":{}}"#);
    wire.extend_from_slice(b"junk_post_terminal_bytes");
    let mut decoder = DevinDecoder::new();
    let mut events = Vec::new();
    decoder.decode(&wire, &mut events);
    assert!(
        !events.iter().any(|e| matches!(e, CodexEvent::Completed { .. })),
        "same-chunk terminal followed by junk must NEVER emit Completed: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, CodexEvent::Failed { message } if message.contains("trailing"))),
        "must emit Failed for trailing data: {events:?}"
    );

    // (3b) Split chunks: chunk 1 valid terminal, chunk 2 extra data -> chunk 1 does NOT emit Completed until EOF; chunk 2 must fail
    let mut decoder = DevinDecoder::new();
    let mut events1 = Vec::new();
    decoder.decode(&frame_end_stream(r#"{"metadata":{}}"#), &mut events1);
    assert!(
        !events1.iter().any(|e| matches!(e, CodexEvent::Completed { .. })),
        "chunk 1 must not emit Completed before stream finish (no premature completion): {events1:?}"
    );

    let mut events2 = Vec::new();
    decoder.decode(b"extra_chunk_after_terminal", &mut events2);
    assert!(
        events2.iter().any(|e| matches!(e, CodexEvent::Failed { message } if message.contains("trailing"))),
        "subsequent chunks after EndStream must fail: {events2:?}"
    );

    let mut events3 = Vec::new();
    decoder.finish(&mut events3);
    assert!(
        !events3.iter().any(|e| matches!(e, CodexEvent::Completed { .. })),
        "failed stream must never complete in finish: {events3:?}"
    );

    // (3c) Clean stream: chunk 1 valid terminal -> finish() emits Completed exactly once (idempotent)
    let mut clean_decoder = DevinDecoder::new();
    let mut clean_events1 = Vec::new();
    clean_decoder.decode(&frame_end_stream(r#"{"metadata":{}}"#), &mut clean_events1);
    assert!(
        !clean_events1.iter().any(|e| matches!(e, CodexEvent::Completed { .. })),
        "no premature completion before EOF"
    );
    let mut clean_events2 = Vec::new();
    clean_decoder.finish(&mut clean_events2);
    assert!(
        clean_events2.iter().any(|e| matches!(e, CodexEvent::Completed { .. })),
        "finish must emit Completed for clean stream: {clean_events2:?}"
    );
    // Idempotent: second finish call does not re-emit Completed
    let mut clean_events3 = Vec::new();
    clean_decoder.finish(&mut clean_events3);
    assert!(
        clean_events3.is_empty(),
        "finish must be idempotent: {clean_events3:?}"
    );

    // (3d) Partial post-terminal bytes rejected at finish()
    let mut decoder = DevinDecoder::new();
    let mut events = Vec::new();
    decoder.decode(&frame_end_stream(r#"{"metadata":{}}"#), &mut events);
    // feed 2 partial bytes
    decoder.decode(&[0u8, 0u8], &mut events);
    decoder.finish(&mut events);
    assert!(
        events.iter().any(|e| matches!(e, CodexEvent::Failed { .. })),
        "partial post-terminal bytes must cause finish() to fail: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(e, CodexEvent::Completed { .. })),
        "corrupted stream must never emit Completed: {events:?}"
    );

    // (3e) Scalar/array JSON EndStream must be rejected as malformed, never Completed
    let array_frame = frame_end_stream(r#"[1, 2, 3]"#);
    let (events, _) = decode_all(&array_frame);
    assert!(
        !events.iter().any(|e| matches!(e, CodexEvent::Completed { .. })),
        "array EndStream JSON must not emit Completed: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, CodexEvent::Failed { .. })),
        "array EndStream JSON must emit Failed: {events:?}"
    );

    let scalar_frame = frame_end_stream(r#""hello world""#);
    let (events, _) = decode_all(&scalar_frame);
    assert!(
        !events.iter().any(|e| matches!(e, CodexEvent::Completed { .. })),
        "scalar EndStream JSON must not emit Completed: {events:?}"
    );
    assert!(
        events.iter().any(|e| matches!(e, CodexEvent::Failed { .. })),
        "scalar EndStream JSON must emit Failed: {events:?}"
    );
}

#[tokio::test]
async fn defect_3_compat_stream_rejects_same_chunk_and_split_terminal_junk() {
    use mahoquot_gateway::compat::{streaming_body, Protocol, ProtocolSession, ReplyShape, StreamingBodyParams};
    use axum::body::to_bytes;

    // Combined chunk: terminal + junk -> compat stream must terminate with error, not normal DONE chunk
    let mut bad_chunk = frame_end_stream(r#"{"metadata":{}}"#);
    bad_chunk.extend_from_slice(b"junk");
    let stream: mahoquot_gateway::compat::UpstreamStream = Box::pin(futures::stream::empty());
    let body = streaming_body(StreamingBodyParams {
        first: Bytes::from(bad_chunk),
        upstream: stream,
        model: "devin/glm-x".into(),
        created: 1,
        include_usage: false,
        shape: ReplyShape::Chat,
        session: ProtocolSession {
            protocol: Protocol::Devin,
            cursor_reply: None,
        },
        upstream_capture: None,
        devin_outcome: None,
    });
    let collected = to_bytes(body, 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&collected);
    assert!(
        text.contains("trailing data after EndStream frame") || text.contains("error"),
        "combined chunk must emit error in stream: {text}"
    );
    assert!(
        !text.contains("\"finish_reason\":\"stop\""),
        "combined chunk must not emit successful stop chunk: {text}"
    );

    // Split chunk: first terminal, second junk
    let stream: mahoquot_gateway::compat::UpstreamStream = Box::pin(futures::stream::iter(vec![
        Ok(Bytes::from_static(b"junk_split")),
    ]));
    let body = streaming_body(StreamingBodyParams {
        first: Bytes::from(frame_end_stream(r#"{"metadata":{}}"#)),
        upstream: stream,
        model: "devin/glm-x".into(),
        created: 1,
        include_usage: false,
        shape: ReplyShape::Chat,
        session: ProtocolSession {
            protocol: Protocol::Devin,
            cursor_reply: None,
        },
        upstream_capture: None,
        devin_outcome: None,
    });
    let collected = to_bytes(body, 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&collected);
    assert!(
        text.contains("trailing data after EndStream frame") || text.contains("error"),
        "split trailing chunk must emit error: {text}"
    );
    assert!(
        !text.contains("\"finish_reason\":\"stop\""),
        "split trailing chunk must not emit successful stop chunk: {text}"
    );
}

#[test]
fn defect_4_delayed_id_and_name_resolution_and_interleaved_tools_with_renderers() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::{ChatMessageResponse, ChatToolCall};
    use mahoquot_gateway::compat::render::{Aggregator, ChunkRenderer, GeminiChunkRenderer};

    // (4a) ID-less tool delta followed by real ID resolves the pending call without allocating second state
    // and args do NOT emit before ToolCallBegin
    let mut wire = Vec::new();
    for d in [
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: None,
                name: Some("search_web".into()),
                arguments_json: Some("{\"query\":".into()),
            }],
            ..Default::default()
        },
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: Some("call_real_id_999".into()),
                name: None,
                arguments_json: Some("\"rust\"}".into()),
            }],
            ..Default::default()
        },
    ] {
        let mut buf = Vec::new();
        d.encode(&mut buf).unwrap();
        wire.extend(frame_data(&buf));
    }
    wire.extend(frame_end_stream(r#"{"metadata":{}}"#));

    let (events, _) = decode_all(&wire);

    // Verify exactly ONE ToolCallBegin was emitted, with call_real_id_999 and search_web
    let begins: Vec<_> = events.iter().filter_map(|e| match e {
        CodexEvent::ToolCallBegin { output_index, call_id, name } => Some((*output_index, call_id.clone(), name.clone())),
        _ => None,
    }).collect();
    assert_eq!(
        begins,
        vec![(0, "call_real_id_999".to_string(), "search_web".to_string())],
        "must resolve pending call with actual id, not generate call_0 or allocate second tool: {events:?}"
    );

    // Verify no ToolArgsDelta was emitted BEFORE ToolCallBegin
    let begin_idx = events.iter().position(|e| matches!(e, CodexEvent::ToolCallBegin { .. })).unwrap();
    let first_arg_idx = events.iter().position(|e| matches!(e, CodexEvent::ToolArgsDelta { .. }));
    if let Some(arg_idx) = first_arg_idx {
        assert!(
            arg_idx > begin_idx,
            "ToolArgsDelta must not appear before ToolCallBegin: {events:?}"
        );
    }

    // (4b) Unresolved tool call at terminal (missing ID) must fail
    let mut bad_wire = Vec::new();
    let d = ChatMessageResponse {
        delta_tool_calls: vec![ChatToolCall {
            id: None,
            name: Some("calc".into()),
            arguments_json: Some("{\"x\":1}".into()),
        }],
        ..Default::default()
    };
    let mut buf = Vec::new();
    d.encode(&mut buf).unwrap();
    bad_wire.extend(frame_data(&buf));
    bad_wire.extend(frame_end_stream(r#"{"metadata":{}}"#));
    let (bad_events, _) = decode_all(&bad_wire);
    assert!(
        bad_events.iter().any(|e| matches!(e, CodexEvent::Failed { message } if message.contains("unresolved"))),
        "unresolved tool missing ID at terminal must fail: {bad_events:?}"
    );
    assert!(
        !bad_events.iter().any(|e| matches!(e, CodexEvent::Completed { .. })),
        "unresolved tool must never emit Completed: {bad_events:?}"
    );

    // (4c) Unresolved tool call at terminal (missing NAME) must fail
    let mut bad_name_wire = Vec::new();
    let d = ChatMessageResponse {
        delta_tool_calls: vec![ChatToolCall {
            id: Some("call_missing_name".into()),
            name: None,
            arguments_json: Some("{\"x\":1}".into()),
        }],
        ..Default::default()
    };
    let mut buf = Vec::new();
    d.encode(&mut buf).unwrap();
    bad_name_wire.extend(frame_data(&buf));
    bad_name_wire.extend(frame_end_stream(r#"{"metadata":{}}"#));
    let (bad_name_events, _) = decode_all(&bad_name_wire);
    assert!(
        bad_name_events.iter().any(|e| matches!(e, CodexEvent::Failed { message } if message.contains("unresolved"))),
        "unresolved tool missing NAME at terminal must fail: {bad_name_events:?}"
    );
    assert!(
        !bad_name_events.iter().any(|e| matches!(e, CodexEvent::Completed { .. })),
        "unresolved tool must never emit Completed: {bad_name_events:?}"
    );

    // (4d) Positive interleaved tools with explicitly identifiable IDs and delayed names
    let mut interleaved_wire = Vec::new();
    for d in [
        // Tool 0 arrives with ID and partial args, delayed name
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: Some("call_tool_0".into()),
                name: None,
                arguments_json: Some("{\"url\":".into()),
            }],
            ..Default::default()
        },
        // Tool 1 arrives with ID and partial args, delayed name
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: Some("call_tool_1".into()),
                name: None,
                arguments_json: Some("{\"code\":".into()),
            }],
            ..Default::default()
        },
        // Tool 0 resolves with name and remaining args
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: Some("call_tool_0".into()),
                name: Some("fetch_doc".into()),
                arguments_json: Some("\"https://example.com\"}".into()),
            }],
            ..Default::default()
        },
        // Tool 1 resolves with name and remaining args
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: Some("call_tool_1".into()),
                name: Some("run_code".into()),
                arguments_json: Some("42}".into()),
            }],
            ..Default::default()
        },
    ] {
        let mut buf = Vec::new();
        d.encode(&mut buf).unwrap();
        interleaved_wire.extend(frame_data(&buf));
    }
    interleaved_wire.extend(frame_end_stream(r#"{"metadata":{}}"#));
    let (interleaved_events, _) = decode_all(&interleaved_wire);

    // Exercise OpenAI ChunkRenderer
    let mut chunk_renderer = ChunkRenderer::new("devin/glm-x".into(), 1, false);
    let mut chunk_frames = Vec::new();
    for ev in &interleaved_events {
        chunk_frames.extend(chunk_renderer.render(ev.clone()));
    }
    assert!(!chunk_frames.is_empty());

    // Exercise GeminiChunkRenderer
    let mut gemini_renderer = GeminiChunkRenderer::new("devin/glm-x".into(), 1);
    let mut gemini_frames = Vec::new();
    for ev in &interleaved_events {
        gemini_frames.extend(gemini_renderer.render(ev.clone()));
    }
    let parsed_gemini: Vec<Value> = gemini_frames
        .iter()
        .filter_map(|f| {
            let s = String::from_utf8_lossy(f);
            s.strip_prefix("data: ")
                .and_then(|json_str| serde_json::from_str(json_str.trim()).ok())
        })
        .collect();
    assert!(
        !parsed_gemini.iter().any(|v| v.get("error").is_some()),
        "Gemini stream must not contain error frames: {parsed_gemini:?}"
    );
    let mut gemini_fc = Vec::new();
    for frame in &parsed_gemini {
        if let Some(candidates) = frame.get("candidates").and_then(Value::as_array) {
            for c in candidates {
                if let Some(parts) = c.get("content").and_then(|ct| ct.get("parts")).and_then(Value::as_array) {
                    for part in parts {
                        if let Some(fc) = part.get("functionCall") {
                            gemini_fc.push(fc.clone());
                        }
                    }
                }
            }
        }
    }
    assert_eq!(gemini_fc.len(), 2, "must emit both function calls: {parsed_gemini:?}");
    let gfc0 = gemini_fc.iter().find(|fc| fc["id"] == "call_tool_0").expect("call_tool_0");
    assert_eq!(gfc0["name"], "fetch_doc");
    assert_eq!(gfc0["args"], json!({"url": "https://example.com"}));
    let gfc1 = gemini_fc.iter().find(|fc| fc["id"] == "call_tool_1").expect("call_tool_1");
    assert_eq!(gfc1["name"], "run_code");
    assert_eq!(gfc1["args"], json!({"code": 42}));

    // Exercise Aggregator
    let mut agg = Aggregator::new("devin/glm-x".into(), 1);
    for ev in interleaved_events {
        agg.push(ev);
    }
    let completion = agg.into_completion();
    let tool_calls = completion["choices"][0]["message"]["tool_calls"].as_array().unwrap();
    assert_eq!(tool_calls.len(), 2);
    assert_eq!(tool_calls[0]["id"], "call_tool_0");
    assert_eq!(tool_calls[0]["function"]["name"], "fetch_doc");
    assert_eq!(tool_calls[0]["function"]["arguments"], "{\"url\":\"https://example.com\"}");
    assert_eq!(tool_calls[1]["id"], "call_tool_1");
    assert_eq!(tool_calls[1]["function"]["name"], "run_code");
    assert_eq!(tool_calls[1]["function"]["arguments"], "{\"code\":42}");

    // (4e) Negative test: ambiguous correlation where identity cannot be resolved fails
    let mut ambiguous_wire = Vec::new();
    for d in [
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: None,
                name: Some("tool_a".into()),
                arguments_json: Some("{\"x\":".into()),
            }],
            ..Default::default()
        },
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: Some("call_tool_1".into()),
                name: None,
                arguments_json: Some("{\"y\":".into()),
            }],
            ..Default::default()
        },
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: Some("call_tool_0".into()),
                name: None,
                arguments_json: Some("1}".into()),
            }],
            ..Default::default()
        },
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: Some("call_tool_1".into()),
                name: Some("tool_b".into()),
                arguments_json: Some("2}".into()),
            }],
            ..Default::default()
        },
    ] {
        let mut buf = Vec::new();
        d.encode(&mut buf).unwrap();
        ambiguous_wire.extend(frame_data(&buf));
    }
    ambiguous_wire.extend(frame_end_stream(r#"{"metadata":{}}"#));
    let (ambiguous_events, _) = decode_all(&ambiguous_wire);
    assert!(
        ambiguous_events.iter().any(|e| matches!(e, CodexEvent::Failed { .. })),
        "ambiguous tool correlation must fail: {ambiguous_events:?}"
    );
    assert!(
        !ambiguous_events.iter().any(|e| matches!(e, CodexEvent::Completed { .. })),
        "ambiguous tool correlation must never complete successfully"
    );
}

#[test]
fn defect_4f_gemini_interleaved_tool_calls_and_text_stream_preserves_both_calls_and_args() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::{ChatMessageResponse, ChatToolCall};
    use mahoquot_gateway::compat::render::GeminiChunkRenderer;

    let mut interleaved_wire = Vec::new();
    for d in [
        // Chunk 1: Tool A begins with partial args, followed by text delta
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: Some("call_a".into()),
                name: Some("func_a".into()),
                arguments_json: Some("{\"city\":".into()),
            }],
            delta_text: Some("thinking about city... ".into()),
            ..Default::default()
        },
        // Chunk 2: Tool B begins with partial args, followed by text delta
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: Some("call_b".into()),
                name: Some("func_b".into()),
                arguments_json: Some("{\"query\":".into()),
            }],
            delta_text: Some("and query... ".into()),
            ..Default::default()
        },
        // Chunk 3: Tool A finishes args
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: Some("call_a".into()),
                name: None,
                arguments_json: Some("\"Tokyo\"}".into()),
            }],
            ..Default::default()
        },
        // Chunk 4: Tool B finishes args, followed by text delta
        ChatMessageResponse {
            delta_tool_calls: vec![ChatToolCall {
                id: Some("call_b".into()),
                name: None,
                arguments_json: Some("\"weather\"}".into()),
            }],
            delta_text: Some("finished".into()),
            ..Default::default()
        },
    ] {
        let mut buf = Vec::new();
        d.encode(&mut buf).unwrap();
        interleaved_wire.extend(frame_data(&buf));
    }
    interleaved_wire.extend(frame_end_stream(r#"{"metadata":{}}"#));

    let (events, _) = decode_all(&interleaved_wire);

    let mut gemini_renderer = GeminiChunkRenderer::new("devin/glm-x".into(), 1);
    let mut gemini_frames = Vec::new();
    for ev in &events {
        gemini_frames.extend(gemini_renderer.render(ev.clone()));
    }

    let parsed_frames: Vec<Value> = gemini_frames
        .iter()
        .filter_map(|f| {
            let s = String::from_utf8_lossy(f);
            s.strip_prefix("data: ")
                .and_then(|json_str| serde_json::from_str(json_str.trim()).ok())
        })
        .collect();

    assert!(
        !parsed_frames.is_empty(),
        "Gemini renderer must emit frames"
    );
    assert!(
        !parsed_frames.iter().any(|v| v.get("error").is_some()),
        "Gemini stream must not contain error frames for valid interleaved tools: {parsed_frames:?}"
    );

    let mut function_calls = Vec::new();
    let mut text_parts = Vec::new();
    for frame in &parsed_frames {
        if let Some(candidates) = frame.get("candidates").and_then(Value::as_array) {
            for c in candidates {
                if let Some(parts) = c.get("content").and_then(|ct| ct.get("parts")).and_then(Value::as_array) {
                    for part in parts {
                        if let Some(fc) = part.get("functionCall") {
                            function_calls.push(fc.clone());
                        }
                        if let Some(t) = part.get("text").and_then(Value::as_str) {
                            text_parts.push(t.to_string());
                        }
                    }
                }
            }
        }
    }

    assert_eq!(
        function_calls.len(),
        2,
        "must emit exactly 2 functionCall parts: {parsed_frames:?}"
    );

    let fc_a = function_calls.iter().find(|fc| fc["id"] == "call_a").expect("call_a must be present");
    assert_eq!(fc_a["name"], "func_a");
    assert_eq!(fc_a["args"], json!({"city": "Tokyo"}));

    let fc_b = function_calls.iter().find(|fc| fc["id"] == "call_b").expect("call_b must be present");
    assert_eq!(fc_b["name"], "func_b");
    assert_eq!(fc_b["args"], json!({"query": "weather"}));

    let joined_text = text_parts.join("");
    assert!(joined_text.contains("thinking about city... "));
    assert!(joined_text.contains("and query... "));
    assert!(joined_text.contains("finished"));
}

#[test]
fn defect_5_response_format_strict_and_json_schema_are_rejected() {
    let p = params();
    // json_schema type must be rejected
    let json_schema_body = json!({
        "messages": [{"role": "user", "content": "hi"}],
        "response_format": {"type": "json_schema", "json_schema": {"name": "schema_x"}}
    });
    assert!(
        build_chat_request(&json_schema_body, &p, &mut counter()).is_err(),
        "response_format json_schema must be rejected"
    );

    // strict true in response_format must be rejected
    let strict_body = json!({
        "messages": [{"role": "user", "content": "hi"}],
        "response_format": {"type": "json_object", "strict": true}
    });
    assert!(
        build_chat_request(&strict_body, &p, &mut counter()).is_err(),
        "response_format strict=true must be rejected"
    );

    // json_schema.strict true must be rejected
    let nested_strict_body = json!({
        "messages": [{"role": "user", "content": "hi"}],
        "response_format": {"json_schema": {"strict": true}}
    });
    assert!(
        build_chat_request(&nested_strict_body, &p, &mut counter()).is_err(),
        "response_format nested strict=true must be rejected"
    );
}

#[test]
fn defect_6_malformed_tool_arguments_preserved_or_rejected_across_all_render_paths() {
    use prost::Message;
    use mahoquot_gateway::compat::devin_proto::{ChatMessageResponse, ChatToolCall};
    use mahoquot_gateway::compat::render::{Aggregator, ChunkRenderer, GeminiChunkRenderer};

    let mut wire = Vec::new();
    let d = ChatMessageResponse {
        stop_reason: Some(10),
        delta_tool_calls: vec![ChatToolCall {
            id: Some("c1".into()),
            name: Some("f".into()),
            arguments_json: Some("{malformed_json_without_closing_brace".into()),
        }],
        ..Default::default()
    };
    let mut buf = Vec::new();
    d.encode(&mut buf).unwrap();
    wire.extend(frame_data(&buf));
    wire.extend(frame_end_stream(r#"{"metadata":{}}"#));

    let (events, _) = decode_all(&wire);

    // Path 1: Aggregator::into_completion preserves malformed string verbatim
    let mut agg = Aggregator::new("devin/glm-x".into(), 1);
    for ev in &events {
        agg.push(ev.clone());
    }
    let completion = agg.into_completion();
    let raw_args = completion["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"].as_str().unwrap();
    assert_eq!(raw_args, "{malformed_json_without_closing_brace");
    assert_ne!(raw_args, "{}");

    // Path 2: ChunkRenderer emits the delta string verbatim
    let mut chunk_renderer = ChunkRenderer::new("devin/glm-x".into(), 1, false);
    let mut chunk_frames = Vec::new();
    for ev in &events {
        chunk_frames.extend(chunk_renderer.render(ev.clone()));
    }
    let chunk_text = chunk_frames.iter().map(|f| String::from_utf8_lossy(f)).collect::<Vec<_>>().join("");
    assert!(chunk_text.contains("{malformed_json_without_closing_brace"));

    // Path 3: GeminiChunkRenderer surfaces structured failure for malformed arguments (never {} or string)
    let mut gemini_renderer = GeminiChunkRenderer::new("devin/glm-x".into(), 1);
    let mut gemini_frames = Vec::new();
    for ev in &events {
        gemini_frames.extend(gemini_renderer.render(ev.clone()));
    }
    let gemini_text = gemini_frames.iter().map(|f| String::from_utf8_lossy(f)).collect::<Vec<_>>().join("");
    assert!(
        !gemini_text.contains("\"args\":{}"),
        "GeminiChunkRenderer must never silently repair malformed args to empty object {{}}: {gemini_text}"
    );
    assert!(
        !gemini_text.contains("\"args\":\"{malformed"),
        "GeminiChunkRenderer must not emit string for functionCall.args: {gemini_text}"
    );
    // Parse Gemini frames: must contain structured error and NO functionCall part
    let parsed_frames: Vec<Value> = gemini_frames
        .iter()
        .filter_map(|f| {
            let s = String::from_utf8_lossy(f);
            s.strip_prefix("data: ")
                .and_then(|json_str| serde_json::from_str(json_str.trim()).ok())
        })
        .collect();
    assert!(
        parsed_frames.iter().any(|v| v.get("error").is_some()),
        "Gemini stream must contain structured error for malformed args: {parsed_frames:?}"
    );
    for frame in &parsed_frames {
        if let Some(candidates) = frame.get("candidates").and_then(Value::as_array) {
            for c in candidates {
                if let Some(parts) = c.get("content").and_then(|content| content.get("parts")).and_then(Value::as_array) {
                    for part in parts {
                        assert!(
                            part.get("functionCall").is_none(),
                            "Gemini candidates must not contain functionCall with malformed args: {part:?}"
                        );
                    }
                }
            }
        }
    }

    // Path 4: GeminiChunkRenderer with valid non-object JSON (null, array, string) also surfaces structured failure
    for non_object_json in ["null", "[1, 2, 3]", "\"string_arg\""] {
        let non_obj_events = vec![
            CodexEvent::ToolCallBegin {
                output_index: 0,
                call_id: "c2".into(),
                name: "func_non_obj".into(),
            },
            CodexEvent::ToolArgsDelta {
                output_index: 0,
                delta: non_object_json.into(),
            },
            CodexEvent::Completed { usage: None },
        ];
        let mut r = GeminiChunkRenderer::new("devin/glm-x".into(), 1);
        let mut frames = Vec::new();
        for ev in non_obj_events {
            frames.extend(r.render(ev));
        }
        let parsed_non_obj: Vec<Value> = frames
            .iter()
            .filter_map(|f| {
                let s = String::from_utf8_lossy(f);
                s.strip_prefix("data: ")
                    .and_then(|json_str| serde_json::from_str(json_str.trim()).ok())
            })
            .collect();
        assert!(
            parsed_non_obj.iter().any(|v| v.get("error").is_some()),
            "Gemini must surface structured error for non-object JSON '{non_object_json}': {parsed_non_obj:?}"
        );
    }

    // Path 5: GeminiChunkRenderer with valid object JSON succeeds with proper functionCall part
    let valid_obj_events = vec![
        CodexEvent::ToolCallBegin {
            output_index: 0,
            call_id: "c3".into(),
            name: "func_valid".into(),
        },
        CodexEvent::ToolArgsDelta {
            output_index: 0,
            delta: "{\"valid\":true}".into(),
        },
        CodexEvent::Completed { usage: None },
    ];
    let mut valid_r = GeminiChunkRenderer::new("devin/glm-x".into(), 1);
    let mut valid_frames = Vec::new();
    for ev in valid_obj_events {
        valid_frames.extend(valid_r.render(ev));
    }
    let parsed_valid: Vec<Value> = valid_frames
        .iter()
        .filter_map(|f| {
            let s = String::from_utf8_lossy(f);
            s.strip_prefix("data: ")
                .and_then(|json_str| serde_json::from_str(json_str.trim()).ok())
        })
        .collect();
    let has_func_call = parsed_valid.iter().any(|v| {
        v.get("candidates")
            .and_then(Value::as_array)
            .map(|cands| {
                cands.iter().any(|c| {
                    c.get("content")
                        .and_then(|ct| ct.get("parts"))
                        .and_then(Value::as_array)
                        .map(|parts| {
                            parts.iter().any(|p| {
                                p.get("functionCall")
                                    .and_then(|fc| fc.get("args"))
                                    .map(|args| args.get("valid") == Some(&Value::Bool(true)))
                                    .unwrap_or(false)
                            })
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    });
    assert!(has_func_call, "Gemini must emit valid functionCall with object args: {parsed_valid:?}");
}


use mahoquot_gateway::compat::{
    self,
    events::{CodexEvent, SseParser, Usage},
    render::Aggregator,
    responses::ResponsesStreamRenderer,
    Protocol, ReplyShape,
};
use prost::Message;
use serde_json::{json, Value};

fn payloads(frames: Vec<bytes::Bytes>) -> Vec<Value> {
    frames
        .iter()
        .filter_map(|frame| {
            std::str::from_utf8(frame)
                .unwrap()
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .and_then(|data| serde_json::from_str(data).ok())
        })
        .collect()
}

#[test]
fn gt1_invalid_gemini_arguments_fail_aggregation() {
    for args in ["{oops", "[]", "null", "12", "\"text\""] {
        let mut aggregate = Aggregator::new("fixture".into(), 1);
        aggregate.push(CodexEvent::ToolCallBegin {
            output_index: 0,
            call_id: "call".into(),
            name: "tool".into(),
        });
        aggregate.push(CodexEvent::ToolArgsDelta {
            output_index: 0,
            delta: args.into(),
        });
        aggregate.push(CodexEvent::Completed { usage: None });
        assert!(aggregate.into_gemini().is_err(), "{args}");
        let raw = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\n",
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call","name":"tool"}}),
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":args}),
            json!({"type":"response.completed","response":{}})
        );
        assert!(compat::aggregate(
            raw.as_bytes(),
            "fixture".into(),
            1,
            Protocol::Codex,
            ReplyShape::Gemini
        )
        .is_err());
    }
}

#[test]
fn gt3_media_and_idless_history_survive_conversion() {
    for text in [None, Some("describe")] {
        let mut parts = vec![json!({"inlineData":{"mimeType":"image/png","data":"YWJj"}})];
        if let Some(text) = text {
            parts.insert(0, json!({"text":text}));
        }
        let request = json!({"contents":[{"role":"user","parts":parts},
            {"role":"model","parts":[{"functionCall":{"name":"weather","args":{}}}]},
            {"role":"user","parts":[{"functionResponse":{"name":"weather","response":{"ok":true}}}]}]});
        let out = compat::gemini::gemini_to_openai(&request, "fixture").unwrap();
        let content = out["messages"][0]["content"].as_array().unwrap();
        assert_eq!(
            content.last().unwrap()["image_url"]["url"],
            "data:image/png;base64,YWJj"
        );
        assert_eq!(
            out["messages"][1]["tool_calls"][0]["id"],
            out["messages"][2]["tool_call_id"]
        );
    }
    let ambiguous = json!({"contents":[{"role":"model","parts":[
        {"functionCall":{"name":"same","args":{}}},{"functionCall":{"name":"same","args":{}}}]},
        {"parts":[{"functionResponse":{"name":"same","response":{}}}]}]});
    assert!(compat::gemini::gemini_to_openai(&ambiguous, "fixture").is_err());
    let unsupported =
        json!({"contents":[{"parts":[{"inlineData":{"mimeType":"audio/wav","data":"YWJj"}}]}]});
    assert!(compat::gemini::gemini_to_openai(&unsupported, "fixture").is_err());
}

#[test]
fn gt4_schema_names_and_enum_literals_are_not_keywords() {
    let literal = json!({"const":1,"default":2,"additionalProperties":3,"$defs":4});
    let properties = json!({"const":{"type":"string"},"default":{"type":"string"},
        "additionalProperties":{"type":"string"},"$defs":{"type":"object","enum":[literal]}});
    let request = json!({"model":"gemini-2.5-pro","messages":[{"role":"user","content":"hi"}],
        "tools":[{"type":"function","function":{"name":"test","parameters":{
            "type":"object","properties":properties,"additionalProperties":false,"default":{},
            "required":["const","default","additionalProperties","$defs"]}}}]});
    let out = compat::gemini::openai_to_gemini(&request).unwrap();
    let schema = &out["tools"][0]["functionDeclarations"][0]["parameters"];
    assert_eq!(schema["properties"], properties);
    assert!(schema.get("additionalProperties").is_none());
    assert!(schema.get("default").is_none());
}

#[test]
fn gt5_stream_indices_and_item_lifetimes_match_final_output() {
    let mut renderer = ResponsesStreamRenderer::new("fixture".into(), 1);
    let events = vec![
        CodexEvent::ReasoningDelta("think".into()),
        CodexEvent::TextDelta("before".into()),
        CodexEvent::ToolCallBegin {
            output_index: 9,
            call_id: "call".into(),
            name: "tool".into(),
        },
        CodexEvent::ToolArgsDelta {
            output_index: 9,
            delta: "{}".into(),
        },
        CodexEvent::TextDelta("after".into()),
        CodexEvent::Completed { usage: None },
    ];
    let mut frames = Vec::new();
    for event in events {
        frames.extend(renderer.render(event));
    }
    let final_response = renderer.current_response();
    let output = final_response["output"].as_array().unwrap();
    assert_eq!(output.len(), 4);
    assert_eq!(output[1]["content"][0]["text"], "before");
    assert_eq!(output[3]["content"][0]["text"], "after");
    let mut open = std::collections::BTreeSet::new();
    let mut done = std::collections::BTreeSet::new();
    for frame in payloads(frames) {
        let Some(index) = frame["output_index"].as_u64() else {
            continue;
        };
        let id = &output[index as usize]["id"];
        match frame["type"].as_str().unwrap() {
            "response.output_item.added" => {
                assert!(open.insert(index));
                assert_eq!(&frame["item"]["id"], id);
            }
            "response.output_item.done" => {
                assert!(open.remove(&index));
                assert!(done.insert(index));
                assert_eq!(&frame["item"], &output[index as usize]);
            }
            _ => {
                assert!(open.contains(&index));
                assert_eq!(&frame["item_id"], id);
            }
        }
    }
    assert!(open.is_empty());
    assert_eq!(done.len(), output.len());
}

#[test]
fn gt6_anthropic_error_is_terminal() {
    let mut decoder = compat::claude::AnthropicDecoder::new();
    let mut events = Vec::new();
    for frame in [
        json!({"type":"message_start","message":{"id":"m"}}),
        json!({"type":"error","error":{"message":"failed"}}),
        json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"wrong"}}),
        json!({"type":"message_stop"}),
    ] {
        decoder.decode(frame.to_string().as_bytes(), &mut events);
    }
    decoder.finish(&mut events);
    assert_eq!(events.len(), 2);
    assert!(matches!(events[1], CodexEvent::Failed { .. }));
    let mut renderer = compat::claude::AnthropicStreamRenderer::new("fixture".into(), 1);
    renderer.render(CodexEvent::Failed {
        message: "failed".into(),
    });
    assert!(renderer
        .render(CodexEvent::TextDelta("wrong".into()))
        .is_empty());
    assert!(renderer
        .render(CodexEvent::Completed { usage: None })
        .is_empty());
    assert!(renderer.close_unterminated().is_empty());
}

#[test]
fn gt7_codex_and_anthropic_require_terminal_events() {
    let terminal = b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n";
    for end in 0..terminal.len() {
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        parser.push(
            b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
            &mut events,
        );
        parser.push(&terminal[..end], &mut events);
        parser.finish(&mut events);
        // A complete JSON terminal can be flushed at EOF without the SSE blank line.
        let complete_json =
            serde_json::from_slice::<Value>(&terminal[6..end.max(6)]).is_ok() && end >= 6;
        if !complete_json {
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e, CodexEvent::Failed { .. }))
                    .count(),
                1
            );
            assert!(!events
                .iter()
                .any(|e| matches!(e, CodexEvent::Completed { .. })));
        } else {
            assert_eq!(
                events
                    .iter()
                    .filter(|e| matches!(e, CodexEvent::Completed { .. }))
                    .count(),
                1
            );
            assert!(!events
                .iter()
                .any(|e| matches!(e, CodexEvent::Failed { .. })));
        }
    }
    let mut decoder = compat::claude::AnthropicDecoder::new();
    let mut events = Vec::new();
    decoder.decode(
        br#"{"type":"message_start","message":{"id":"m"}}"#,
        &mut events,
    );
    decoder.finish(&mut events);
    decoder.finish(&mut events);
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, CodexEvent::Failed { .. }))
            .count(),
        1
    );
    assert!(!events
        .iter()
        .any(|e| matches!(e, CodexEvent::Completed { .. })));
}

#[test]
fn gt7_cursor_partial_frame_eof_fails_once() {
    let frame =
        compat::cursor::connect_frame(&compat::cursor_fixture_turn_end().encode_to_vec(), 0);
    for end in 0..frame.len() {
        let mut decoder = compat::cursor::CursorDecoder::new();
        let mut events = Vec::new();
        decoder.decode(&frame[..end], &mut events);
        decoder.finish(&mut events);
        decoder.finish(&mut events);
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, CodexEvent::Failed { .. }))
                .count(),
            1
        );
        assert!(!events
            .iter()
            .any(|e| matches!(e, CodexEvent::Completed { .. })));
    }
}

#[test]
fn terminal_event_is_not_followed_by_coalesced_line_limit_failure() {
    let mut raw = b"data: {\"type\":\"response.completed\",\"response\":{}}\n\n".to_vec();
    raw.extend(std::iter::repeat_n(
        b'x',
        compat::events::MAX_SSE_LINE_BYTES + 1,
    ));
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    parser.push(&raw, &mut events);
    parser.finish(&mut events);
    assert_eq!(events, vec![CodexEvent::Completed { usage: None }]);
}

#[test]
fn gt8_cache_presence_matches_streamed_and_nonstream_usage() {
    for metric in [None, Some(0), Some(17)] {
        let events = vec![CodexEvent::Completed {
            usage: Some(Usage {
                prompt_tokens: 20,
                completion_tokens: 2,
                total_tokens: 22,
                cached_tokens: metric.unwrap_or(0),
                cached_tokens_known: metric.is_some(),
                cache_write_tokens: metric.unwrap_or(0),
                cache_write_tokens_known: metric.is_some(),
                reasoning_tokens: 0,
            }),
        }];
        let response = compat::responses::responses_response(&events, "fixture", 1).unwrap();
        let mut renderer = ResponsesStreamRenderer::new("fixture".into(), 1);
        let frames = payloads(renderer.render(events[0].clone()));
        let streamed = &frames.last().unwrap()["response"];
        assert_eq!(streamed, &response);
        for field in ["cached_tokens", "cache_write_tokens"] {
            assert_eq!(
                response["usage"]["input_tokens_details"][field].as_u64(),
                metric
            );
        }
    }
}

#[tokio::test]
async fn gt6_streaming_body_terminal_is_independent_of_chunk_boundaries() {
    let raw = concat!(
        "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"message\":\"fixture failure\"}}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"wrong\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{}}\n\n"
    ).as_bytes();
    let mut expected = None;
    for split in 0..=raw.len() {
        let tail = bytes::Bytes::copy_from_slice(&raw[split..]);
        let upstream: compat::UpstreamStream = Box::pin(futures::stream::iter([Ok(tail)]));
        let body = compat::streaming_body(compat::StreamingBodyParams {
            first: bytes::Bytes::copy_from_slice(&raw[..split]),
            upstream,
            model: "fixture".into(),
            created: 1,
            include_usage: false,
            shape: ReplyShape::Anthropic,
            session: compat::ProtocolSession {
                protocol: Protocol::Codex,
                cursor_reply: None,
                replay: None,
                gemini_tool_names: None,
            },
            upstream_capture: None,
            devin_outcome: None,
        });
        let bytes = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            axum::body::to_bytes(body, 65536),
        )
        .await
        .unwrap()
        .unwrap();
        let frames = payloads(vec![bytes.clone()]);
        let wire = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(wire.matches("event: error\n").count(), 1);
        assert!(!wire.contains("event: message_stop"));
        assert!(!wire.contains("wrong"));
        assert!(!frames.is_empty());
        if let Some(expected) = &expected {
            assert_eq!(&bytes, expected);
        } else {
            expected = Some(bytes);
        }
    }
}

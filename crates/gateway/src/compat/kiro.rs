use serde_json::{json, Map, Value};

use super::events::CodexEvent;

pub fn openai_to_kiro(body: &Value) -> Result<Value, String> {
    openai_to_kiro_with_profile(body, None)
}

pub fn openai_to_kiro_with_profile(
    body: &Value,
    profile_arn: Option<&str>,
) -> Result<Value, String> {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing model".to_string())?;
    let model = model.strip_prefix("kiro/").unwrap_or(model);
    let model = if model == "auto-kiro" { "auto" } else { model };
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| "missing messages".to_string())?;

    let mut system = Vec::new();
    let mut conversational = Vec::new();
    for message in messages {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or("user");
        let content = text_content(message.get("content").unwrap_or(&Value::Null));
        if role == "system" || role == "developer" {
            if !content.is_empty() {
                system.push(content);
            }
            continue;
        }
        conversational.push((role, content, message));
    }
    if conversational.is_empty() {
        return Err("messages contain no user turn".to_string());
    }

    let mut tool_results = Vec::new();
    let mut current_images = Vec::new();
    while conversational
        .last()
        .is_some_and(|(role, _, _)| *role == "tool")
    {
        let (_, content, message) = conversational.pop().unwrap();
        let id = message
            .get("tool_call_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "Kiro tool result missing tool_call_id".to_string())?;
        let text = if content.trim().is_empty() {
            "Tool completed without textual output".to_string()
        } else {
            content
        };
        current_images.extend(images_content(
            message.get("content").unwrap_or(&Value::Null),
        ));
        tool_results.push(json!({
            "toolUseId": normalize_tool_id(id),
            "content": [{"text": text}],
            "status": if message.get("is_error").and_then(Value::as_bool) == Some(true) {
                "error"
            } else {
                "success"
            },
        }));
    }
    tool_results.reverse();

    let mut current = String::new();
    if let Some((current_role, current_text, current_message)) = conversational.pop() {
        if current_role == "user" {
            current = current_text;
            current_images.extend(images_content(
                current_message.get("content").unwrap_or(&Value::Null),
            ));
        } else {
            conversational.push((current_role, current_text, current_message));
        }
    }
    if current.is_empty() && !tool_results.is_empty() {
        current = "Tool results are available in the message context".to_string();
    }
    if !system.is_empty() {
        current = if current.is_empty() {
            system.join("\n\n")
        } else {
            format!("{}\n\n{}", system.join("\n\n"), current)
        };
    }

    let mut history = Vec::new();
    for (role, content, message) in conversational {
        if role == "assistant" {
            let mut assistant = json!({ "content": content });
            if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
                assistant["toolUses"] = Value::Array(
                    calls
                        .iter()
                        .map(|call| {
                            let arguments = call["function"]["arguments"]
                                .as_str()
                                .and_then(|raw| serde_json::from_str(raw).ok())
                                .unwrap_or_else(|| json!({}));
                            json!({
                                "toolUseId": normalize_tool_id(call["id"].as_str().unwrap_or_default()),
                                "name": call["function"]["name"],
                                "input": arguments,
                            })
                        })
                        .collect(),
                );
            }
            if let Some(reasoning) = message
                .get("kiroRedactedReasoning")
                .or_else(|| message.get("kiro_redacted_reasoning"))
                .and_then(Value::as_str)
            {
                assistant["reasoningContent"] = json!({"redactedContent": reasoning});
            }
            history.push(json!({ "assistantResponseMessage": assistant }));
        } else {
            history.push(json!({
                "userInputMessage": {
                    "content": content,
                    "modelId": model,
                    "origin": "AI_EDITOR",
                }
            }));
        }
    }

    let mut context = Map::new();
    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        let specifications: Vec<Value> = tools
            .iter()
            .filter_map(|tool| tool.get("function"))
            .map(|function| {
                let mut schema = function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({}));
                sanitize_schema(&mut schema);
                json!({
                    "toolSpecification": {
                        "name": function["name"],
                        "description": function["description"],
                        "inputSchema": { "json": schema },
                    }
                })
            })
            .collect();
        if !specifications.is_empty() {
            context.insert("tools".to_string(), Value::Array(specifications));
        }
    }

    if !tool_results.is_empty() {
        context.insert("toolResults".to_string(), Value::Array(tool_results));
    }

    let mut user_input = json!({
        "content": current,
        "modelId": model,
        "origin": "AI_EDITOR",
        "userInputMessageContext": Value::Object(context),
    });
    if !current_images.is_empty() {
        user_input["images"] = Value::Array(current_images);
    }

    let mut payload = json!({
        "conversationState": {
            "chatTriggerType": "MANUAL",
            "conversationId": format!("{:016x}", rand::random::<u64>()),
            "currentMessage": {
                "userInputMessage": user_input
            },
            "history": history,
        }
    });
    if let Some(profile_arn) = profile_arn.filter(|value| !value.is_empty()) {
        payload["profileArn"] = Value::String(profile_arn.to_string());
    }
    Ok(payload)
}

fn normalize_tool_id(id: &str) -> String {
    id.replace('|', "_")
}

fn text_content(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

fn images_content(value: &Value) -> Vec<Value> {
    let Value::Array(blocks) = value else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| {
            let url = block
                .get("image_url")
                .and_then(|image| image.get("url"))
                .or_else(|| block.get("imageUrl"))
                .and_then(Value::as_str)?;
            let encoded = url.strip_prefix("data:image/")?;
            let (format, bytes) = encoded.split_once(";base64,")?;
            let format = if format == "jpg" { "jpeg" } else { format };
            matches!(format, "jpeg" | "png" | "gif" | "webp")
                .then(|| json!({"format": format, "source": {"bytes": bytes}}))
        })
        .collect()
}

fn sanitize_schema(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("additionalProperties");
            if map
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            {
                map.remove("required");
            }
            for child in map.values_mut() {
                sanitize_schema(child);
            }
        }
        Value::Array(values) => values.iter_mut().for_each(sanitize_schema),
        _ => {}
    }
}

#[derive(Default)]
pub struct KiroDecoder {
    wire: Vec<u8>,
    framed: Option<bool>,
    buffer: String,
    /// Bytes of a character split across chunk boundaries, awaiting completion.
    pending: Vec<u8>,
    completed: bool,
    current_tool: Option<(String, String, u64)>,
}

impl KiroDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn decode(&mut self, bytes: &[u8], out: &mut Vec<CodexEvent>) {
        if self.completed {
            return;
        }
        self.wire.extend_from_slice(bytes);
        if self.framed.is_none() {
            let Some(first) = self.wire.iter().find(|byte| !byte.is_ascii_whitespace()) else {
                return;
            };
            self.framed = Some(*first != b'{');
        }
        if self.framed == Some(false) {
            let bytes = std::mem::take(&mut self.wire);
            self.decode_json(&bytes, out);
            return;
        }
        while self.wire.len() >= 12 && !self.completed {
            let total = u32::from_be_bytes(self.wire[..4].try_into().unwrap()) as usize;
            let headers = u32::from_be_bytes(self.wire[4..8].try_into().unwrap()) as usize;
            if total < 16 || headers > total - 16 {
                self.fail("Invalid Kiro event-stream framing".to_string(), out);
                return;
            }
            if self.wire.len() < total {
                return;
            }
            let frame: Vec<u8> = self.wire.drain(..total).collect();
            let mut cursor = 12;
            let header_end = 12 + headers;
            let mut error_type = None;
            while cursor < header_end {
                let name_len = frame[cursor] as usize;
                cursor += 1;
                if cursor + name_len + 3 > header_end || frame[cursor + name_len] != 7 {
                    self.fail("Invalid Kiro event-stream headers".to_string(), out);
                    return;
                }
                let name = &frame[cursor..cursor + name_len];
                cursor += name_len + 1;
                let len = u16::from_be_bytes(frame[cursor..cursor + 2].try_into().unwrap()) as usize;
                cursor += 2;
                if cursor + len > header_end {
                    self.fail("Invalid Kiro event-stream headers".to_string(), out);
                    return;
                }
                let value = &frame[cursor..cursor + len];
                if name == b":message-type" && (value == b"exception" || value == b"error") {
                    error_type = Some(String::from_utf8_lossy(value).into_owned());
                }
                cursor += len;
            }
            let payload = &frame[header_end..total - 4];
            if let Some(error_type) = error_type {
                let value = serde_json::from_slice(payload).unwrap_or(Value::Null);
                self.fail(kiro_error_message(&value).unwrap_or(error_type), out);
            } else {
                self.decode_json(payload, out);
            }
        }
    }

    fn fail(&mut self, message: String, out: &mut Vec<CodexEvent>) {
        self.completed = true;
        out.push(CodexEvent::Failed { message });
    }

    fn decode_json(&mut self, bytes: &[u8], out: &mut Vec<CodexEvent>) {
        // Chunks arrive unaligned to UTF-8 boundaries, so decode from the
        // accumulated bytes: converting each chunk on its own would replace a
        // trailing partial character with U+FFFD before the next chunk can
        // complete it.
        self.pending.extend_from_slice(bytes);
        let decoded = match std::str::from_utf8(&self.pending) {
            Ok(text) => {
                let text = text.to_string();
                self.pending.clear();
                text
            }
            Err(error) => {
                let valid_upto = error.valid_up_to();
                // A genuine invalid sequence (not a split character) would
                // otherwise wedge the buffer forever, so only a trailing
                // incomplete character is held back.
                if error.error_len().is_some() {
                    let text = String::from_utf8_lossy(&self.pending).to_string();
                    self.pending.clear();
                    text
                } else {
                    let text = String::from_utf8_lossy(&self.pending[..valid_upto]).to_string();
                    self.pending.drain(..valid_upto);
                    text
                }
            }
        };
        self.buffer.push_str(&decoded);
        while let Some((start, end)) = next_json_object(&self.buffer) {
            let candidate = self.buffer[start..=end].to_string();
            self.buffer.drain(..=end);
            let Ok(value) = serde_json::from_str::<Value>(&candidate) else {
                self.fail("Invalid Kiro JSON payload".to_string(), out);
                return;
            };
            if value.get("__type").is_some() || value.get("error").is_some() {
                self.fail(kiro_error_message(&value).unwrap_or_else(|| "Kiro upstream error".to_string()), out);
                return;
            } else if let Some(content) = value.get("content").and_then(Value::as_str) {
                out.push(CodexEvent::TextDelta(content.to_string()));
            } else if let (Some(name), Some(id)) = (
                value.get("name").and_then(Value::as_str),
                value.get("toolUseId").and_then(Value::as_str),
            ) {
                let index = self.current_tool.as_ref().map_or(0, |tool| tool.2 + 1);
                self.current_tool = Some((id.to_string(), name.to_string(), index));
                out.push(CodexEvent::ToolCallBegin {
                    output_index: index,
                    call_id: id.to_string(),
                    name: name.to_string(),
                });
                if let Some(input) = value.get("input") {
                    out.push(CodexEvent::ToolArgsDelta {
                        output_index: index,
                        delta: input
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| input.to_string()),
                    });
                }
            } else if let Some(input) = value.get("input").and_then(Value::as_str) {
                if let Some((_, _, index)) = &self.current_tool {
                    out.push(CodexEvent::ToolArgsDelta {
                        output_index: *index,
                        delta: input.to_string(),
                    });
                }
            } else if let Some(text) = value.get("text").and_then(Value::as_str) {
                out.push(CodexEvent::ReasoningDelta(text.to_string()));
            } else if let Some(signature) = value.get("signature").and_then(Value::as_str) {
                out.push(CodexEvent::ReasoningSignature(signature.to_string()));
            } else if value.get("stopReason").is_some() {
                self.completed = true;
                out.push(CodexEvent::Completed { usage: None });
                return;
            }
        }
    }

    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        if !self.completed {
            if !self.wire.is_empty() || !self.pending.is_empty() || !self.buffer.trim().is_empty() {
                self.fail("Truncated Kiro stream".to_string(), out);
                return;
            }
            out.push(CodexEvent::Completed { usage: None });
            self.completed = true;
        }
    }
}

fn kiro_error_message(value: &Value) -> Option<String> {
    [value.get("message"), value.get("Message"), value.pointer("/error/message"),
        value.get("__type"), value.get("error")]
        .into_iter()
        .flatten()
        .find_map(Value::as_str)
        .map(str::to_string)
}

fn next_json_object(input: &str) -> Option<(usize, usize)> {
    let start = input.find('{')?;
    let mut depth = 0u32;
    let mut string = false;
    let mut escaped = false;
    for (offset, ch) in input[start..].char_indices() {
        if string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                string = false;
            }
            continue;
        }
        match ch {
            '"' => string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((start, start + offset));
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aws_frame(message_type: &str, payload: &[u8]) -> Vec<u8> {
        fn crc(bytes: &[u8]) -> u32 {
            let mut crc = !0u32;
            for byte in bytes {
                crc ^= u32::from(*byte);
                for _ in 0..8 {
                    crc = (crc >> 1) ^ (0xedb88320 & (0u32.wrapping_sub(crc & 1)));
                }
            }
            !crc
        }
        let mut headers = vec![13];
        headers.extend_from_slice(b":message-type");
        headers.push(7);
        headers.extend_from_slice(&(message_type.len() as u16).to_be_bytes());
        headers.extend_from_slice(message_type.as_bytes());
        let mut frame = ((16 + headers.len() + payload.len()) as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&crc(&frame).to_be_bytes());
        frame.extend(headers);
        frame.extend_from_slice(payload);
        frame.extend_from_slice(&crc(&frame).to_be_bytes());
        frame
    }

    #[test]
    fn clean_aws_eof_completes_without_stop_reason_at_every_split() {
        let mut wire = aws_frame("event", br#"{"content":"hello"}"#);
        wire.extend(aws_frame("event", br#"{"name":"lookup","toolUseId":"clean-eof","input":"{}"}"#));
        for split in 0..=wire.len() {
            let mut decoder = KiroDecoder::new();
            let mut events = Vec::new();
            decoder.decode(&wire[..split], &mut events);
            decoder.decode(&wire[split..], &mut events);
            decoder.finish(&mut events);
            decoder.finish(&mut events);
            assert_eq!(events.first(), Some(&CodexEvent::TextDelta("hello".into())));
            assert_eq!(events.last(), Some(&CodexEvent::Completed { usage: None }));
            assert_eq!(events.iter().filter(|event| matches!(event, CodexEvent::Completed { .. })).count(), 1);
            assert!(!events.iter().any(|event| matches!(event, CodexEvent::Failed { .. })));
        }
    }

    #[test]
    fn error_payload_emits_failed_event_and_prevents_completed() {
        for (payload, expected) in [
            (r#"{"__type":"ValidationException","message":"Token limit exceeded"}"#, "Token limit exceeded"),
            (r#"{"error":"AccessDeniedException","Message":"Denied"}"#, "Denied"),
            (r#"{"error":{"message":"Nested denial"}}"#, "Nested denial"),
            (r#"{"__type":"ValidationException"}"#, "ValidationException"),
            (r#"{"error":"AccessDeniedException"}"#, "AccessDeniedException"),
        ] {
            let mut decoder = KiroDecoder::new();
            let mut events = Vec::new();
            decoder.decode(payload.as_bytes(), &mut events);
            decoder.decode(br#"{"stopReason":"end_turn"}{"content":"late"}"#, &mut events);
            decoder.finish(&mut events);
            assert!(matches!(events.as_slice(), [CodexEvent::Failed { message }] if message == expected), "{payload}: {events:?}");
        }
    }

    #[test]
    fn aws_error_frames_fail_at_every_chunk_boundary_after_text() {
        for message_type in ["exception", "error"] {
            let text = aws_frame("event", br#"{"content":"committed"}"#);
            let error = aws_frame(message_type, br#"{"message":"Denied"}"#);
            let stop = aws_frame("event", br#"{"stopReason":"end_turn"}"#);
            let wire = [text, error, stop].concat();
            for split in 0..=wire.len() {
                let mut decoder = KiroDecoder::new();
                let mut events = Vec::new();
                decoder.decode(&wire[..split], &mut events);
                decoder.decode(&wire[split..], &mut events);
                decoder.finish(&mut events);
                assert!(matches!(events.as_slice(), [CodexEvent::TextDelta(text), CodexEvent::Failed { message }] if text == "committed" && message == "Denied"), "{message_type} split {split}: {events:?}");
                let mut renderer = super::super::render::ChunkRenderer::new("kiro/test".to_string(), 0, false);
                let frames: Vec<_> = events.into_iter().flat_map(|event| renderer.render(event)).collect();
                let payloads: Vec<Value> = frames.iter()
                    .filter(|frame| frame.as_ref() != super::super::render::DONE_FRAME)
                    .map(|frame| serde_json::from_slice(&frame[6..frame.len() - 2]).unwrap())
                    .collect();
                assert_eq!(payloads.last().unwrap()["error"]["message"], "Denied");
                assert!(payloads.iter().all(|payload| payload["choices"][0]["finish_reason"].is_null()));
                assert!(renderer.terminated());
                assert!(renderer.close_unterminated().is_empty());
            }
        }
    }

    #[test]
    fn normal_message_and_partial_aws_frames_are_not_errors() {
        let wire = [
            aws_frame("event", br#"{"message":"metadata"}"#),
            aws_frame("event", br#"{"content":"hello {world}"}"#),
        ].concat();
        for split in 0..=wire.len() {
            let mut decoder = KiroDecoder::new();
            let mut events = Vec::new();
            decoder.decode(&wire[..split], &mut events);
            decoder.decode(&wire[split..], &mut events);
            decoder.finish(&mut events);
            assert!(matches!(events.as_slice(), [CodexEvent::TextDelta(text), CodexEvent::Completed { .. }] if text == "hello {world}"), "split {split}: {events:?}");
        }
    }

    #[test]
    fn truncated_aws_frame_fails_instead_of_completing() {
        let wire = aws_frame("event", br#"{"content":"hello"}"#);
        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(&wire[..wire.len() - 1], &mut events);
        decoder.finish(&mut events);
        assert!(matches!(events.as_slice(), [CodexEvent::Failed { .. }]), "{events:?}");
    }

    #[test]
    fn multibyte_split_across_chunks_survives_reassembly() {
        // Network chunks are not aligned to UTF-8 boundaries. Converting each
        // chunk with from_utf8_lossy replaces the trailing partial sequence
        // with U+FFFD before the buffer can rejoin it, corrupting CJK/emoji
        // text and tool-argument JSON.
        let full: Vec<u8> = br#"{"content":""#
            .iter()
            .copied()
            .chain("あい".bytes())
            .chain(br#""}"#.iter().copied())
            .collect();
        // Split inside the first multi-byte character.
        let split = full.iter().position(|b| *b == 0xe3).expect("multibyte start") + 1;
        let mut decoder = KiroDecoder::new();
        let mut out = Vec::new();
        decoder.decode(&full[..split], &mut out);
        decoder.decode(&full[split..], &mut out);
        let text: String = out
            .iter()
            .filter_map(|event| match event {
                CodexEvent::TextDelta(delta) => Some(delta.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "あい", "chunk split corrupted the text");
        assert!(!text.contains('�'), "decoder emitted replacement chars");
    }

    #[test]
    fn decodes_streamed_tool_call_into_codex_events() {
        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            br#"{"name":"lookup","toolUseId":"call_1"}{"input":"{\"q\":\"x\"}","stop":true}"#,
            &mut events,
        );
        assert!(events.iter().any(|event| matches!(event, CodexEvent::ToolCallBegin { call_id, name, .. } if call_id == "call_1" && name == "lookup")));
        assert!(events.iter().any(
            |event| matches!(event, CodexEvent::ToolArgsDelta { delta, .. } if delta.contains("q"))
        ));
    }

    #[test]
    fn replays_tool_results_images_and_reasoning_on_kiro_wire() {
        let payload = openai_to_kiro(&json!({
            "model": "kiro/claude-haiku-4-5-20251001",
            "messages": [
                {"role":"user","content":"look"},
                {"role":"assistant","content":"", "kiroRedactedReasoning":"blob", "tool_calls":[{
                    "id":"call|1", "type":"function",
                    "function":{"name":"inspect","arguments":"{}"}
                }]},
                {"role":"tool","tool_call_id":"call|1","content":[
                    {"type":"text","text":"done"},
                    {"type":"image_url","image_url":{"url":"data:image/jpg;base64,abc"}}
                ]}
            ]
        }))
        .unwrap();
        let state = &payload["conversationState"];
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["toolUses"][0]["toolUseId"],
            "call_1"
        );
        assert_eq!(
            state["history"][1]["assistantResponseMessage"]["reasoningContent"]["redactedContent"],
            "blob"
        );
        let current = &state["currentMessage"]["userInputMessage"];
        assert_eq!(
            current["userInputMessageContext"]["toolResults"][0]["toolUseId"],
            "call_1"
        );
        assert_eq!(
            current["userInputMessageContext"]["toolResults"][0]["content"][0]["text"],
            "done"
        );
        assert_eq!(current["images"][0]["format"], "jpeg");
        assert_eq!(current["images"][0]["source"]["bytes"], "abc");
    }

    #[test]
    fn profile_reasoning_and_object_tool_input_follow_reference_wire() {
        let payload = openai_to_kiro_with_profile(
            &json!({
                "model":"kiro/claude-sonnet-4.6",
                "messages":[{"role":"user","content":"hi"}]
            }),
            Some("arn:aws:codewhisperer:us-east-1:123:profile/abc"),
        )
        .expect("translation");
        assert_eq!(
            payload["profileArn"],
            "arn:aws:codewhisperer:us-east-1:123:profile/abc"
        );

        let mut decoder = KiroDecoder::new();
        let mut events = Vec::new();
        decoder.decode(
            br#"{"text":"internal"}{"name":"bash","toolUseId":"call_1","input":{"cmd":"ls"}}"#,
            &mut events,
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event, CodexEvent::TextDelta(text) if text == "internal")));
        assert!(events.iter().any(
            |event| matches!(event, CodexEvent::ToolArgsDelta { delta, .. } if delta.contains("ls"))
        ));
    }
}

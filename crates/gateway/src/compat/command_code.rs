//! Command Code OAuth CLI wire format (not the OpenAI-compatible API-key API).
use super::events::{CodexEvent, Usage};
use serde_json::{json, Value};

pub fn openai_to_command_code(body: &Value) -> Result<Value, String> {
    let messages = body["messages"].as_array().ok_or("missing messages")?;
    let model = body["model"].as_str().ok_or("missing model")?;
    let system = messages
        .iter()
        .filter(|m| matches!(m["role"].as_str(), Some("system" | "developer")))
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut wire_messages = Vec::new();
    let mut calls = std::collections::HashMap::<String, String>::new();
    for message in messages {
        let role = message["role"].as_str().unwrap_or("user");
        if matches!(role, "system" | "developer") {
            continue;
        }
        let mut content = Vec::new();
        match &message["content"] {
            Value::String(text) => content.push(json!({"type":"text","text":text})),
            Value::Array(parts) => {
                for part in parts {
                    if let Some(url) = part.pointer("/image_url/url").and_then(Value::as_str) {
                        let mut image = json!({"type":"image","image":url});
                        if let Some((mime, _)) = url
                            .strip_prefix("data:")
                            .and_then(|url| url.split_once(';'))
                        {
                            image["mediaType"] = json!(mime);
                        }
                        content.push(image);
                    } else if let Some(text) = part.get("text").and_then(Value::as_str) {
                        content.push(json!({"type":"text","text":text}));
                    }
                }
            }
            _ => {}
        }
        if role == "assistant" {
            if let Some(reasoning) = message["reasoning_content"].as_str() {
                content.push(json!({"type":"reasoning", "text":reasoning}));
            }
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                let id = call["id"].as_str().ok_or("tool call missing id")?;
                let name = call["function"]["name"]
                    .as_str()
                    .ok_or("tool call missing name")?;
                calls.insert(id.into(), name.into());
                let input = call["function"]["arguments"]
                    .as_str()
                    .map(serde_json::from_str::<Value>)
                    .transpose()
                    .map_err(|_| "invalid tool arguments")?
                    .unwrap_or(json!({}));
                content.push(
                    json!({"type":"tool-call", "toolCallId":id,"toolName":name,"input":input}),
                );
            }
        } else if role == "tool" {
            let id = message["tool_call_id"]
                .as_str()
                .ok_or("tool result missing id")?;
            let name = calls
                .remove(id)
                .ok_or("tool result without matching call")?;
            let text = content
                .iter()
                .map(|part| part["text"].as_str().unwrap_or("[image]"))
                .collect::<Vec<_>>()
                .join("");
            content = vec![
                json!({"type":"tool-result", "toolCallId":id, "toolName":name,
                "output":{"type":if message["is_error"].as_bool() == Some(true) {"error-text"} else {"text"},"value":text}}),
            ];
        }
        wire_messages.push(json!({"role": role, "content": content}));
    }
    if !calls.is_empty() {
        return Err("Command Code assistant tool call missing result".into());
    }
    let tools = body["tools"].as_array().into_iter().flatten().filter_map(|tool| tool.get("function")).map(|function|
        json!({"name":function["name"],"description":function["description"],"input_schema":function.get("parameters").cloned().unwrap_or(json!({}))})).collect::<Vec<_>>();
    let mut params = json!({"model": model.strip_prefix("command-code/").unwrap_or(model),
        "messages": wire_messages, "tools": tools, "system": system,
        "stream": true, "max_tokens": body.get("max_tokens").or_else(|| body.get("max_completion_tokens")).cloned().unwrap_or(json!(64000))});
    for key in ["temperature", "reasoning_effort"] {
        if let Some(value) = body.get(key) {
            params[key] = value.clone();
        }
    }
    Ok(
        json!({"config": {}, "memory": "", "taste": null, "skills": null,
        "permissionMode": "standard", "mode": "agent", "params": params}),
    )
}

pub fn command_code_headers(token: &str, session_id: &str) -> Vec<(&'static str, String)> {
    vec![
        ("authorization", format!("Bearer {token}")),
        ("content-type", "application/json".into()),
        ("user-agent", "cli".into()),
        ("x-command-code-version", "0.52.1".into()),
        ("x-cli-environment", "production".into()),
        ("x-taste-learning", "false".into()),
        ("x-co-flag", "false".into()),
        ("x-session-id", session_id.into()),
    ]
}

#[derive(Default)]
pub struct CommandCodeDecoder {
    buffer: Vec<u8>,
    completed: bool,
    next_tool: u64,
}
impl CommandCodeDecoder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn decode(&mut self, bytes: &[u8], out: &mut Vec<CodexEvent>) {
        if self.completed {
            return;
        }
        for byte in bytes {
            if *byte == b'\n' {
                let line = std::mem::take(&mut self.buffer);
                self.record(&line, out);
                if self.completed {
                    return;
                }
            } else {
                self.buffer.push(*byte);
                if self.buffer.len() > super::events::MAX_SSE_LINE_BYTES {
                    self.completed = true;
                    out.push(CodexEvent::Failed {
                        message: "Command Code NDJSON line exceeds limit".into(),
                    });
                    return;
                }
            }
        }
    }
    fn record(&mut self, line: &[u8], out: &mut Vec<CodexEvent>) {
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        let value: Value = match serde_json::from_slice(line) {
            Ok(value) => value,
            Err(_) => {
                self.completed = true;
                out.push(CodexEvent::Failed {
                    message: "Invalid Command Code NDJSON".into(),
                });
                return;
            }
        };
        match value["type"].as_str().unwrap_or_default() {
            "text-delta" => {
                if let Some(text) = value["text"].as_str() {
                    out.push(CodexEvent::TextDelta(text.into()));
                }
            }
            "reasoning-delta" => {
                if let Some(text) = value["text"].as_str() {
                    out.push(CodexEvent::ReasoningDelta(text.into()));
                }
            }
            "tool-call" => {
                let index = self.next_tool;
                self.next_tool += 1;
                out.push(CodexEvent::ToolCallBegin {
                    output_index: index,
                    call_id: value["toolCallId"]
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("command-code-{index}")),
                    name: value["toolName"].as_str().unwrap_or("tool").into(),
                });
                let input = value
                    .get("input")
                    .or_else(|| value.get("args"))
                    .cloned()
                    .unwrap_or(json!({}));
                out.push(CodexEvent::ToolArgsDelta {
                    output_index: index,
                    delta: input
                        .as_str()
                        .map(str::to_string)
                        .unwrap_or_else(|| input.to_string()),
                });
            }
            "finish" | "finish-step" => {
                self.completed = true;
                if value
                    .get("rawFinishReason")
                    .or_else(|| value.get("finishReason"))
                    .and_then(Value::as_str)
                    == Some("error")
                {
                    out.push(CodexEvent::Failed {
                        message: "Command Code upstream generation failed".into(),
                    });
                } else {
                    let usage =
                        value
                            .get("totalUsage")
                            .or_else(|| value.get("usage"))
                            .map(|usage| {
                                let input = usage["inputTokens"].as_u64().unwrap_or(0);
                                let output = usage["outputTokens"].as_u64().unwrap_or(0);
                                Usage {
                                    prompt_tokens: input,
                                    completion_tokens: output,
                                    total_tokens: usage["totalTokens"]
                                        .as_u64()
                                        .unwrap_or(input + output),
                                    cached_tokens: 0,
                                    cached_tokens_known: false,
                                    cache_write_tokens: 0,
                                    cache_write_tokens_known: false,
                                    reasoning_tokens: 0,
                                }
                            });
                    out.push(CodexEvent::Completed { usage });
                }
            }
            "error" => {
                self.completed = true;
                out.push(CodexEvent::Failed {
                    message: value["error"]
                        .as_str()
                        .unwrap_or("Command Code upstream error")
                        .into(),
                });
            }
            _ => {}
        }
    }
    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        if self.completed {
            return;
        }
        let line = std::mem::take(&mut self.buffer);
        self.record(&line, out);
        if !self.completed {
            self.completed = true;
            out.push(CodexEvent::Completed { usage: None });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_code_oauth_uses_cli_wire() {
        let wire = openai_to_command_code(&json!({"model":"command-code/test", "messages":[{"role":"system","content":"rules"},{"role":"user","content":"hi"}], "stream":false})).unwrap();
        assert_eq!(wire["params"]["messages"][0]["content"][0]["text"], "hi");
        assert_eq!(wire["params"]["system"], "rules");
        assert_eq!(wire["params"]["stream"], true);
        assert!(command_code_headers("token", "session").contains(&("user-agent", "cli".into())));
        let mut decoder = CommandCodeDecoder::new();
        let mut events = Vec::new();
        decoder.decode(b"{\"type\":\"text-delta\",\"text\":\"hi\"}\n{\"type\":\"reasoning-delta\",\"text\":\"think\"}\n{\"type\":\"tool-call\",\"toolCallId\":\"a\",\"toolName\":\"read\",\"input\":{}}\n{\"type\":\"finish\"}\n", &mut events);
        decoder.finish(&mut events);
        assert!(matches!(events[0], CodexEvent::TextDelta(ref text) if text == "hi"));
        assert!(matches!(events[1], CodexEvent::ReasoningDelta(_)));
        assert!(matches!(events[2], CodexEvent::ToolCallBegin { .. }));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, CodexEvent::Completed { .. }))
                .count(),
            1
        );
    }
}

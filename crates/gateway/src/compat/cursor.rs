use prost::Message;
use serde_json::Value;

use super::cursor_proto as proto;
use super::events::CodexEvent;

/// Upper bound on a single declared Connect frame. The four-byte length prefix
/// is authoritative before any payload arrives, so an oversized declaration is
/// refused as soon as it is read instead of buffering the payload it promises.
pub const MAX_CURSOR_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// Capacity of the bounded channel carrying mandatory client replies (KV/exec)
/// and heartbeats back into the upstream request body.
pub const CURSOR_REPLY_QUEUE_CAPACITY: usize = 64;

/// Reported when a mandatory reply cannot be enqueued because the bounded reply
/// channel is full. The decoder converts this into an explicit failure event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplyQueueFull;

pub fn openai_to_cursor_connect(body: &Value) -> Result<Vec<u8>, String> {
    let requested = body
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing model".to_string())?
        .strip_prefix("cursor/")
        .unwrap_or_else(|| body["model"].as_str().unwrap_or_default());
    let model = cursor_wire_model(requested, body);

    let id = format!("{:016x}", rand::random::<u64>());
    let tools = body
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|tool| tool.get("function"))
        .map(|function| proto::McpToolDefinition {
            name: function["name"].as_str().unwrap_or("tool").to_string(),
            description: function["description"].as_str().unwrap_or("").to_string(),
            input_schema: Some(json_to_protobuf_value(
                &function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({"type": "object"})),
            )),
            provider_identifier: "opencodex-responses".to_string(),
            tool_name: function["name"].as_str().unwrap_or("tool").to_string(),
        })
        .collect();
    let parameter = requested
        .strip_prefix("auto-")
        .map(|level| proto::RequestedModelParameter {
            id: "optimization".to_string(),
            value: level.to_string(),
        });
    let messages = body["messages"]
        .as_array()
        .ok_or_else(|| "Cursor requires a messages array".to_string())?;
    let system_text = messages
        .iter()
        .filter(|message| matches!(message["role"].as_str(), Some("system" | "developer")))
        .map(|message| text_content(message.get("content").unwrap_or(&Value::Null)))
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    // The final conversational message is sent separately as UserMessage.text,
    // so history must drop the last of the *conversational* messages - counting
    // against the whole array would keep it whenever a system or developer
    // message is present and duplicate the current turn.
    let conversational = messages
        .iter()
        .filter(|message| !matches!(message["role"].as_str(), Some("system" | "developer")))
        .collect::<Vec<_>>();
    let user_text = conversational
        .last()
        .and_then(|message| message.get("content"))
        .map(text_content)
        .unwrap_or_default();
    if user_text.is_empty() {
        return Err("Cursor requires a user message".to_string());
    }
    let history = conversational
        .iter()
        .take(conversational.len().saturating_sub(1))
        .filter_map(|message| {
            let role = message["role"].as_str().unwrap_or("user");
            let content = text_content(message.get("content").unwrap_or(&Value::Null));
            if content.is_empty() {
                None
            } else {
                Some(format!("[{role}]\n{content}"))
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let text = [system_text, history, user_text]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    let run = proto::AgentRunRequest {
        conversation_state: Some(proto::ConversationStateStructure {
            root_prompt_messages_json: Vec::new(),
            turns: Vec::new(),
            previous_workspace_uris: vec!["file:///".to_string()],
            mode: Some(1),
            client_name: "mahoquot".to_string(),
        }),
        action: Some(proto::ConversationAction {
            action: Some(proto::conversation_action::Action::UserMessageAction(
                proto::UserMessageAction {
                    user_message: Some(proto::UserMessage {
                        content: Some(proto::UserMessageContent {
                            text: Some(proto::UserMessageText { text }),
                        }),
                        message_id: id.clone(),
                        mode: 1,
                        correlation_id: id.clone(),
                    }),
                },
            )),
        }),
        model_details: Some(proto::ModelDetails {
            model_id: model.clone(),
            display_model_id: model.clone(),
            display_name: model.clone(),
            display_name_short: model.clone(),
            max_mode: Some(requested.ends_with("-1m")),
        }),
        mcp_tools: Some(proto::McpTools { mcp_tools: tools }),
        conversation_id: Some(id),
        custom_system_prompt: None,
        requested_model: Some(proto::RequestedModel {
            model_id: model,
            max_mode: requested.ends_with("-1m"),
            parameters: parameter.into_iter().collect(),
        }),
    };
    let envelope = proto::AgentClientMessage {
        message: Some(proto::agent_client_message::Message::RunRequest(Box::new(
            run,
        ))),
    };
    Ok(connect_frame(&envelope.encode_to_vec(), 0))
}

pub fn client_heartbeat_frame() -> Vec<u8> {
    connect_frame(
        &proto::AgentClientMessage {
            message: Some(proto::agent_client_message::Message::ClientHeartbeat(
                proto::ClientHeartbeat {},
            )),
        }
        .encode_to_vec(),
        0,
    )
}

/// Cursor wire id for one requested model.
///
/// `auto*` stays `default`. Claude Opus 5.5 is a capability id; the live
/// catalog's representative variant is `claude-opus-5-5-medium`, and effort
/// selects `claude-opus-5-5-<level>`. Other ids pass through. OpenCodex and
/// the OMO adapter still describe this as a suffix on the capability id; CPA
/// has no Cursor translator in the vendored upstream.
fn cursor_wire_model(requested: &str, body: &Value) -> String {
    if requested.starts_with("auto") {
        return "default".to_string();
    }
    const CAPABILITY: &str = "claude-opus-5-5";
    const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
    let normalized = requested.replace('.', "-");
    let (base, suffix) = LEVELS
        .iter()
        .find_map(|level| {
            normalized
                .strip_suffix(&format!("-{level}"))
                .filter(|base| *base == CAPABILITY)
                .map(|base| (base, Some(*level)))
        })
        .unwrap_or((normalized.as_str(), None));
    if base != CAPABILITY {
        return requested.to_string();
    }
    let requested_effort = body
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .or_else(|| {
            body.get("reasoning")
                .and_then(|reasoning| reasoning.get("effort"))
                .and_then(Value::as_str)
        });
    let level = requested_effort
        .map(|value| value.to_ascii_lowercase())
        .filter(|value| LEVELS.contains(&value.as_str()))
        .or_else(|| suffix.map(str::to_string))
        .unwrap_or_else(|| "medium".to_string());
    format!("{CAPABILITY}-{level}")
}

fn json_to_protobuf_value(value: &Value) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match value {
        Value::Null => Kind::NullValue(prost_types::NullValue::NullValue as i32),
        Value::Bool(flag) => Kind::BoolValue(*flag),
        Value::Number(number) => Kind::NumberValue(number.as_f64().unwrap_or(0.0)),
        Value::String(text) => Kind::StringValue(text.clone()),
        Value::Array(items) => Kind::ListValue(prost_types::ListValue {
            values: items.iter().map(json_to_protobuf_value).collect(),
        }),
        Value::Object(fields) => Kind::StructValue(prost_types::Struct {
            fields: fields
                .iter()
                .map(|(key, value)| (key.clone(), json_to_protobuf_value(value)))
                .collect(),
        }),
    };
    prost_types::Value { kind: Some(kind) }
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

pub fn connect_frame(payload: &[u8], flags: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 5);
    out.push(flags);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

#[derive(Default)]
pub struct CursorDecoder {
    buffer: Vec<u8>,
    completed: bool,
    output_tokens: u64,
    open_tools: std::collections::HashMap<String, (String, u64)>,
    next_tool_index: u64,
    reply_tx: Option<tokio::sync::mpsc::Sender<bytes::Bytes>>,
}

impl CursorDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_reply_sender(reply_tx: tokio::sync::mpsc::Sender<bytes::Bytes>) -> Self {
        Self {
            reply_tx: Some(reply_tx),
            ..Self::default()
        }
    }

    pub fn decode(&mut self, bytes: &[u8], out: &mut Vec<CodexEvent>) {
        if self.completed {
            return;
        }
        self.buffer.extend_from_slice(bytes);
        while self.buffer.len() >= 5 {
            let flags = self.buffer[0];
            let length = u32::from_be_bytes([
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
                self.buffer[4],
            ]) as usize;
            // The declared length is authoritative before the payload arrives,
            // so an oversized declaration is refused instead of buffering for
            // the frame it promises.
            if length > MAX_CURSOR_FRAME_BYTES {
                self.fail("Cursor frame exceeds the size limit", out);
                return;
            }
            if self.buffer.len() < length + 5 {
                return;
            }
            let payload = self.buffer[5..5 + length].to_vec();
            self.buffer.drain(..5 + length);
            if flags & 0x02 != 0 {
                if let Ok(value) = serde_json::from_slice::<Value>(&payload) {
                    if let Some(message) = value["error"]["message"].as_str() {
                        self.fail(message, out);
                        return;
                    }
                }
                if !self.completed {
                    self.complete(out);
                }
                continue;
            }
            if std::env::var("MAHOQUOT_EXEC_DEBUG").is_ok() {
                let head: Vec<String> = payload
                    .iter()
                    .take(16)
                    .map(|b| format!("{b:02x}"))
                    .collect();
                eprintln!("cursor-frame len={} head={}", payload.len(), head.join(" "));
            }
            let Ok(message) = proto::AgentServerMessage::decode(payload.as_slice()) else {
                if std::env::var_os("MAHOQUOT_EXEC_TRACE").is_some() {
                    let hex: Vec<String> =
                        payload.iter().take(160).map(|b| format!("{b:02x}")).collect();
                    eprintln!(
                        "mahoquot exec trace: undecodable frame len={} bytes={}",
                        payload.len(),
                        hex.join(" ")
                    );
                }
                continue;
            };
            if std::env::var_os("MAHOQUOT_EXEC_TRACE").is_some() {
                if let Some(proto::agent_server_message::Message::ExecServerMessage(exec)) =
                    &message.message
                {
                    if exec.message.is_none() {
                        let hex: Vec<String> =
                            payload.iter().take(200).map(|b| format!("{b:02x}")).collect();
                        eprintln!(
                            "mahoquot exec trace: undecoded exec id={} len={} bytes={}",
                            exec.id,
                            payload.len(),
                            hex.join(" ")
                        );
                    }
                }
            }
            self.decode_message(message, out);
            // A terminal event (turn end or a reply-queue failure) ends the
            // stream: stop decoding instead of consuming the rest forever.
            if self.completed {
                return;
            }
        }
    }

    /// Marks the stream failed and emits exactly one terminal failure event.
    fn fail(&mut self, message: impl Into<String>, out: &mut Vec<CodexEvent>) {
        self.completed = true;
        out.push(CodexEvent::Failed {
            message: message.into(),
        });
    }

    fn decode_message(&mut self, message: proto::AgentServerMessage, out: &mut Vec<CodexEvent>) {
        let Some(message) = message.message else {
            return;
        };
        let update = match message {
            proto::agent_server_message::Message::InteractionUpdate(update) => update,
            proto::agent_server_message::Message::KvServerMessage(message) => {
                self.reply_to_kv(message, out);
                return;
            }
            proto::agent_server_message::Message::ExecServerMessage(message) => {
                self.reply_to_exec(message, out);
                return;
            }
            proto::agent_server_message::Message::ConversationCheckpointUpdate(_) => return,
        };
        match update.message {
            Some(proto::interaction_update::Message::TextDelta(delta)) => {
                if !delta.text.is_empty() {
                    out.push(CodexEvent::TextDelta(delta.text));
                }
            }
            Some(proto::interaction_update::Message::ThinkingDelta(delta)) => {
                if !delta.text.is_empty() {
                    out.push(CodexEvent::ReasoningDelta(delta.text));
                }
            }
            Some(proto::interaction_update::Message::TokenDelta(delta)) => {
                self.output_tokens = self
                    .output_tokens
                    .saturating_add(delta.tokens.max(0) as u64);
            }
            Some(proto::interaction_update::Message::ToolCallStarted(call)) => {
                self.start_tool(call.call_id, call.tool_call, out);
            }
            Some(proto::interaction_update::Message::PartialToolCall(call)) => {
                self.start_tool(call.call_id.clone(), call.tool_call, out);
                if let Some((_, index)) = self.open_tools.get(&call.call_id) {
                    out.push(CodexEvent::ToolArgsDelta {
                        output_index: *index,
                        delta: call.args_text_delta,
                    });
                }
            }
            Some(proto::interaction_update::Message::ToolCallCompleted(call)) => {
                self.start_tool(call.call_id, call.tool_call, out);
            }
            Some(proto::interaction_update::Message::TurnEnded(usage)) => {
                self.completed = true;
                out.push(CodexEvent::Completed {
                    usage: Some(super::events::Usage {
                        prompt_tokens: usage.input_tokens,
                        completion_tokens: usage.output_tokens,
                        total_tokens: usage.input_tokens + usage.output_tokens,
                        cached_tokens: usage.cache_read_tokens + usage.cache_write_tokens,
                        cached_tokens_known: usage.cache_read_tokens > 0
                            || usage.cache_write_tokens > 0,
                        reasoning_tokens: usage.reasoning_tokens,
                        cache_write_tokens: usage.cache_write_tokens,
                        cache_write_tokens_known: usage.cache_write_tokens > 0,
                    }),
                });
            }
            _ => {}
        }
    }

    /// Enqueues a mandatory reply. A full (or closed) bounded queue is a
    /// protocol failure: the decoder terminates and reports it explicitly
    /// rather than silently dropping the reply.
    fn send_reply(
        &mut self,
        message: proto::AgentClientMessage,
        out: &mut Vec<CodexEvent>,
    ) -> Result<(), ReplyQueueFull> {
        let Some(tx) = self.reply_tx.clone() else {
            return Ok(());
        };
        match tx.try_send(bytes::Bytes::from(connect_frame(
            &message.encode_to_vec(),
            0,
        ))) {
            Ok(()) => Ok(()),
            Err(_) => {
                self.fail("cursor reply queue full", out);
                Err(ReplyQueueFull)
            }
        }
    }

    fn reply_to_kv(&mut self, message: proto::KvServerMessage, out: &mut Vec<CodexEvent>) {
        let reply = match message.message {
            Some(proto::kv_server_message::Message::GetBlobArgs(_)) => {
                proto::kv_client_message::Message::GetBlobResult(proto::GetBlobResult {
                    blob_data: None,
                })
            }
            Some(proto::kv_server_message::Message::SetBlobArgs(_)) => {
                proto::kv_client_message::Message::SetBlobResult(proto::SetBlobResult {
                    error: None,
                })
            }
            None => return,
        };
        let _ = self.send_reply(
            proto::AgentClientMessage {
                message: Some(proto::agent_client_message::Message::KvClientMessage(
                    proto::KvClientMessage {
                        id: message.id,
                        message: Some(reply),
                    },
                )),
            },
            out,
        );
    }

    fn reply_to_exec(&mut self, message: proto::ExecServerMessage, out: &mut Vec<CodexEvent>) {
        if std::env::var_os("MAHOQUOT_EXEC_TRACE").is_some() {
            let tag = match &message.message {
                Some(proto::exec_server_message::Message::WriteArgs(_)) => "write",
                Some(proto::exec_server_message::Message::DeleteArgs(_)) => "delete",
                Some(proto::exec_server_message::Message::GrepArgs(_)) => "grep",
                Some(proto::exec_server_message::Message::ReadArgs(_)) => "read",
                Some(proto::exec_server_message::Message::LsArgs(_)) => "ls",
                Some(proto::exec_server_message::Message::ShellArgs(_)) => "shell",
                Some(proto::exec_server_message::Message::ShellStreamArgs(_)) => "shell_stream",
                Some(proto::exec_server_message::Message::McpArgs(_)) => "mcp",

                Some(_) => "other",
                None => "none",
            };
            eprintln!(
                "mahoquot exec trace: forward variant={tag} id={} exec_id={}",
                message.id, message.exec_id
            );
        }
        let id = message.id;
        let exec_id = message.exec_id.clone();
        let tx = self.reply_tx.clone();
        let work = move || {
            let mut send = |frame: proto::AgentClientMessage| {
                let Some(tx) = tx.as_ref() else {
                    return;
                };
                let bytes = bytes::Bytes::from(connect_frame(&frame.encode_to_vec(), 0));
                if tx.blocking_send(bytes).is_err() {
                    return;
                }
            };
            match super::exec_bridge::forward_stream(&message, &mut send) {
                Ok(true) => {}
                Ok(false) => super::exec_ops::handle_with(&message, &mut send),
                Err(error) => {
                    for frame in super::exec_ops::unsupported(id, &exec_id, &error) {
                        send(frame);
                    }
                }
            }
        };
        if tokio::runtime::Handle::try_current().is_ok() {
            tokio::task::spawn_blocking(work);
        } else {
            work();
        }
    }
    fn start_tool(
        &mut self,
        call_id: String,
        tool: Option<proto::ToolCall>,
        out: &mut Vec<CodexEvent>,
    ) {
        // Tool calls surfaced by Cursor's agent are executed by the gateway's own exec
        // handlers; forwarding them downstream would make the client run them a second
        // time (and with a bogus name), so only the resulting text is streamed out.
        let _ = (call_id, tool, out);
        return;
        #[allow(unreachable_code)]
        if self.open_tools.contains_key(&call_id) {
            return;
        }
        let name = tool
            .and_then(|tool| tool.mcp_tool_call)
            .and_then(|tool| tool.args)
            .map(|args| {
                if args.tool_name.is_empty() {
                    args.name
                } else {
                    args.tool_name
                }
            })
            .unwrap_or_else(|| "tool".to_string());
        let index = self.next_tool_index;
        self.next_tool_index += 1;
        self.open_tools
            .insert(call_id.clone(), (name.clone(), index));
        out.push(CodexEvent::ToolCallBegin {
            output_index: index,
            call_id,
            name,
        });
    }

    fn complete(&mut self, out: &mut Vec<CodexEvent>) {
        self.completed = true;
        out.push(CodexEvent::Completed {
            usage: Some(super::events::Usage {
                prompt_tokens: 0,
                completion_tokens: self.output_tokens,
                total_tokens: self.output_tokens,
                cached_tokens: 0,
                cached_tokens_known: false,
                reasoning_tokens: 0,
                cache_write_tokens: 0,
                cache_write_tokens_known: false,
            }),
        });
    }

    pub fn finish(&mut self, out: &mut Vec<CodexEvent>) {
        if !self.completed {
            self.complete(out);
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn system_prompt_does_not_duplicate_the_current_turn() {
        // take(messages.len() - 1) applies a whole-array count to the already
        // filtered conversational subset, so any system/developer message
        // leaves the final user turn in `turns` while it is also sent as the
        // new UserMessage.text.
        let body = serde_json::json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "hi"}
            ]
        });
        let encoded = openai_to_cursor_connect(&body).expect("encodes");
        let text = String::from_utf8_lossy(&encoded);
        let occurrences = text.matches("hi").count();
        assert_eq!(
            occurrences, 1,
            "current turn duplicated into conversation history: {occurrences} copies"
        );
    }
    use super::*;

    #[test]
    fn trailing_system_message_does_not_replace_user_prompt() {
        for role in ["system", "developer"] {
            for root_index in 0..=3 {
                let history = vec![
                    serde_json::json!({"role":"user","content":"earlier question"}),
                    serde_json::json!({"role":"assistant","content":"earlier answer"}),
                ];
                let mut messages = history.clone();
                messages.push(serde_json::json!({"role":"user","content":"search codebase"}));
                let root = serde_json::json!({"role":role,"content":"be concise"});
                messages.insert(root_index, root.clone());
                let body = serde_json::json!({"model":"cursor/auto-cost","messages":messages});
                let frame = openai_to_cursor_connect(&body).unwrap();
                assert_eq!(frame[0], 0);
                assert_eq!(
                    u32::from_be_bytes(frame[1..5].try_into().unwrap()) as usize,
                    frame.len() - 5
                );
                let envelope = proto::AgentClientMessage::decode(&frame[5..]).unwrap();
                let Some(proto::agent_client_message::Message::RunRequest(run)) = envelope.message
                else {
                    panic!("missing run request");
                };
                let Some(proto::conversation_action::Action::UserMessageAction(action)) =
                    run.action.unwrap().action
                else {
                    panic!("missing user action");
                };
                let message = action.user_message.unwrap();
                assert_eq!(
                    message.content.as_ref().and_then(|content| content.text.as_ref()).map(|text| text.text.as_str()),
                    Some("be concise\n\n[user]\nearlier question\n\n[assistant]\nearlier answer\n\nsearch codebase"),
                    "{role} at {root_index}"
                );
                let state = run.conversation_state.unwrap();
                let roots: Vec<Value> = state
                    .root_prompt_messages_json
                    .iter()
                    .map(|bytes| serde_json::from_slice(bytes).unwrap())
                    .collect();
                let turns: Vec<Value> = state
                    .turns
                    .iter()
                    .map(|bytes| serde_json::from_slice(bytes).unwrap())
                    .collect();
                assert!(roots.is_empty(), "{role} at {root_index}");
                assert!(turns.is_empty(), "{role} at {root_index}");
            }
        }
    }

    #[test]
    fn root_prompts_without_conversational_text_are_rejected() {
        for messages in [
            serde_json::json!([]),
            serde_json::json!([{"role":"system","content":"be concise"}]),
            serde_json::json!([{"role":"developer","content":"be concise"}]),
            serde_json::json!([{"role":"user","content":""},{"role":"system","content":"be concise"}]),
        ] {
            let body = serde_json::json!({"model":"cursor/auto-cost","messages":messages});
            assert!(openai_to_cursor_connect(&body).is_err(), "{body}");
        }
    }

    #[test]
    fn a_request_without_messages_is_a_translation_error_not_a_panic() {
        let body = serde_json::json!({"model":"cursor/auto-cost"});
        let error = openai_to_cursor_connect(&body).expect_err("no messages must fail");
        assert!(error.contains("Cursor requires"), "{error}");
    }

    #[test]
    fn request_is_wrapped_in_agent_client_message() {
        let body = serde_json::json!({
            "model":"cursor/auto-cost",
            "messages":[{"role":"user","content":"hello"}],
            "tools":[{"type":"function","function":{"name":"lookup","description":"d","parameters":{"type":"object"}}}]
        });
        let framed = openai_to_cursor_connect(&body).unwrap();
        let envelope = proto::AgentClientMessage::decode(&framed[5..]).unwrap();
        let Some(proto::agent_client_message::Message::RunRequest(run)) = envelope.message else {
            panic!("missing run request");
        };
        assert!(run.action.unwrap().action.is_some());
        assert_eq!(run.requested_model.unwrap().model_id, "default");
        assert_eq!(run.mcp_tools.unwrap().mcp_tools[0].name, "lookup");
    }

    #[test]
    fn server_kv_and_context_requests_receive_matching_client_replies() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(CURSOR_REPLY_QUEUE_CAPACITY);
        let mut decoder = CursorDecoder::with_reply_sender(tx);
        let mut events = Vec::new();
        for message in [
            proto::AgentServerMessage {
                message: Some(proto::agent_server_message::Message::KvServerMessage(
                    proto::KvServerMessage {
                        id: 7,
                        message: Some(proto::kv_server_message::Message::GetBlobArgs(
                            proto::GetBlobArgs { blob_id: vec![1] },
                        )),
                    },
                )),
            },
            proto::AgentServerMessage {
                message: Some(proto::agent_server_message::Message::KvServerMessage(
                    proto::KvServerMessage {
                        id: 8,
                        message: Some(proto::kv_server_message::Message::SetBlobArgs(
                            proto::SetBlobArgs {
                                blob_id: vec![1],
                                blob_data: vec![2],
                            },
                        )),
                    },
                )),
            },
            proto::AgentServerMessage {
                message: Some(proto::agent_server_message::Message::ExecServerMessage(
                    proto::ExecServerMessage {
                        id: 9,
                        exec_id: "exec-9".to_string(),
                        message: Some(proto::exec_server_message::Message::RequestContextArgs(
                            proto::RequestContextArgs {
                                notes_session_id: None,
                                workspace_id: None,
                                use_cached: Some(false),
                            },
                        )),
                    },
                )),
            },
        ] {
            decoder.decode(&connect_frame(&message.encode_to_vec(), 0), &mut events);
        }

        let replies: Vec<_> = (0..3)
            .map(|_| {
                let frame = rx.try_recv().unwrap();
                proto::AgentClientMessage::decode(&frame[5..]).unwrap()
            })
            .collect();
        assert!(matches!(
            &replies[0].message,
            Some(proto::agent_client_message::Message::KvClientMessage(reply))
                if reply.id == 7 && matches!(reply.message, Some(proto::kv_client_message::Message::GetBlobResult(_)))
        ));
        assert!(matches!(
            &replies[1].message,
            Some(proto::agent_client_message::Message::KvClientMessage(reply))
                if reply.id == 8 && matches!(reply.message, Some(proto::kv_client_message::Message::SetBlobResult(_)))
        ));
        assert!(matches!(
            &replies[2].message,
            Some(proto::agent_client_message::Message::ExecClientMessage(reply))
                if reply.id == 9 && reply.exec_id == "exec-9"
        ));
    }

    #[test]
    fn thinking_and_error_trailers_are_not_success_text() {
        let mut decoder = CursorDecoder::new();
        let mut events = Vec::new();
        let thinking = proto::AgentServerMessage {
            message: Some(proto::agent_server_message::Message::InteractionUpdate(
                proto::InteractionUpdate {
                    message: Some(proto::interaction_update::Message::ThinkingDelta(
                        proto::TextDeltaUpdate {
                            text: "internal".into(),
                        },
                    )),
                },
            )),
        };
        decoder.decode(&connect_frame(&thinking.encode_to_vec(), 0), &mut events);
        decoder.decode(
            &connect_frame(
                br#"{"error":{"code":"resource_exhausted","message":"quota exceeded"}}"#,
                2,
            ),
            &mut events,
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event, CodexEvent::TextDelta(text) if text == "internal")));
        assert!(events.iter().any(|event| matches!(event, CodexEvent::Failed { message } if message.contains("quota exceeded"))));
    }

    #[test]
    fn turn_end_usage_is_preserved() {
        let mut decoder = CursorDecoder::new();
        let mut events = Vec::new();
        let ended = proto::AgentServerMessage {
            message: Some(proto::agent_server_message::Message::InteractionUpdate(
                proto::InteractionUpdate {
                    message: Some(proto::interaction_update::Message::TurnEnded(
                        proto::TurnEndedUpdate {
                            input_tokens: 150,
                            output_tokens: 42,
                            cache_read_tokens: 7,
                            cache_write_tokens: 3,
                            reasoning_tokens: 11,
                        },
                    )),
                },
            )),
        };
        decoder.decode(&connect_frame(&ended.encode_to_vec(), 0), &mut events);
        assert!(events.iter().any(
            |event| matches!(event, CodexEvent::Completed { usage: Some(usage) }
            if usage.prompt_tokens == 150 && usage.completion_tokens == 42
                && usage.cached_tokens == 10 && usage.reasoning_tokens == 11)
        ));
    }

    #[test]
    fn cursor_decoder_fails_on_an_oversized_declared_frame() {
        // Only the five-byte prelude is delivered: the declared length must be
        // refused without buffering for the payload it promises.
        let mut prelude = vec![0u8];
        prelude.extend_from_slice(&(MAX_CURSOR_FRAME_BYTES as u32 + 1).to_be_bytes());
        assert_eq!(prelude.len(), 5);

        let mut decoder = CursorDecoder::new();
        let mut events = Vec::new();
        decoder.decode(&prelude, &mut events);
        // A later chunk must not resurrect the stream or add a second failure.
        decoder.decode(b"data: more", &mut events);
        decoder.finish(&mut events);
        assert_eq!(
            events.len(),
            1,
            "expected exactly one terminal failure: {events:?}"
        );
        assert!(matches!(events[0], CodexEvent::Failed { .. }));
    }

    #[test]
    fn cursor_reply_queue_full_surfaces_failure_instead_of_dropping() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<bytes::Bytes>(CURSOR_REPLY_QUEUE_CAPACITY);
        let mut decoder = CursorDecoder::with_reply_sender(tx);
        let mut events = Vec::new();

        // Each KV request demands exactly one reply; with nothing draining the
        // bounded channel, the request that exceeds its capacity must surface an
        // explicit failure rather than a silently dropped mandatory reply.
        for id in 0..CURSOR_REPLY_QUEUE_CAPACITY as u32 + 1 {
            let request = proto::AgentServerMessage {
                message: Some(proto::agent_server_message::Message::KvServerMessage(
                    proto::KvServerMessage {
                        id,
                        message: Some(proto::kv_server_message::Message::GetBlobArgs(
                            proto::GetBlobArgs { blob_id: vec![1] },
                        )),
                    },
                )),
            };
            decoder.decode(&connect_frame(&request.encode_to_vec(), 0), &mut events);
        }

        assert!(
            matches!(events.last(), Some(CodexEvent::Failed { message }) if message.contains("reply queue full")),
            "expected an explicit queue-full failure, got {events:?}"
        );
        let mut delivered = 0;
        while rx.try_recv().is_ok() {
            delivered += 1;
        }
        assert_eq!(
            delivered, CURSOR_REPLY_QUEUE_CAPACITY,
            "every enqueued reply must be delivered before the overflow"
        );
    }
}

#[cfg(test)]
#[cfg(test)]
mod exec_write_tests {
    use super::*;

    fn temp_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("mahoquot-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    #[test]
    fn read_exec_reports_a_missing_file_as_file_not_found() {
        let dir = temp_root("read-missing");
        let missing = dir.join("not-there.txt");
        let result = exec_read(&proto::ReadArgs {
            path: missing.display().to_string(),
        });
        let _ = std::fs::remove_dir_all(&dir);
        let proto::exec_client_message::Message::ReadResult(result) = result else {
            panic!("expected a read result");
        };
        assert!(
            matches!(
                result.result,
                Some(proto::read_result::Result::FileNotFound(_))
            ),
            "a missing file must be reported as file_not_found so Cursor proceeds to write it"
        );
    }

    #[test]
    fn write_exec_creates_the_file_and_echoes_it_when_asked() {
        let dir = temp_root("write-echo");
        let target = dir.join("nested").join("out.txt");
        let result = exec_write(&proto::WriteArgs {
            path: target.display().to_string(),
            file_text: "BRIDGE_WRITE_OK\n".to_string(),
            tool_call_id: String::new(),
            return_file_content_after_write: true,
            file_bytes: Vec::new(),
        });
        let written = std::fs::read_to_string(&target).expect("file exists");
        let _ = std::fs::remove_dir_all(&dir);
        let proto::exec_client_message::Message::WriteResult(result) = result else {
            panic!("expected a write result");
        };
        let Some(proto::write_result::Result::Success(success)) = result.result else {
            panic!("expected a successful write");
        };
        assert_eq!(written, "BRIDGE_WRITE_OK\n");
        assert_eq!(success.file_content_after_write.as_deref(), Some(written.as_str()));
    }

    #[test]
    fn write_exec_prefers_raw_bytes_over_text() {
        let dir = temp_root("write-bytes");
        let target = dir.join("raw.bin");
        let result = exec_write(&proto::WriteArgs {
            path: target.display().to_string(),
            file_text: "ignored".to_string(),
            tool_call_id: String::new(),
            return_file_content_after_write: false,
            file_bytes: b"RAW_BYTES".to_vec(),
        });
        let written = std::fs::read(&target).expect("file exists");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(
            result,
            proto::exec_client_message::Message::WriteResult(_)
        ));
        assert_eq!(written, b"RAW_BYTES");
    }
}

#[cfg(test)]
mod exec_grep_tests {
    use super::*;

    #[test]
    fn grep_finds_a_pattern_through_the_resolved_tool() {
        let dir = std::env::temp_dir().join(format!("mahoquot-grep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("a.txt"), "alpha\nBETA_MARKER\ngamma\n").expect("write");
        let result = exec_grep(&proto::GrepArgs {
            pattern: "BETA_MARKER".to_string(),
            path: Some(dir.display().to_string()),
            glob: None,
            output_mode: Some("content".to_string()),
            context_before: None,
            context_after: None,
            context: None,
            case_insensitive: None,
        });
        let _ = std::fs::remove_dir_all(&dir);
        let proto::exec_client_message::Message::GrepResult(result) = result else {
            panic!("expected a grep result");
        };
        let Some(proto::grep_result::Result::Success(success)) = result.result else {
            panic!("grep must succeed without a hard ripgrep dependency");
        };
        let Some(proto::grep_union_result::Result::Content(content)) = success
            .workspace_results
            .values()
            .next()
            .and_then(|result| result.result.clone())
        else {
            panic!("expected content matches");
        };
        assert_eq!(content.total_matched_lines, 1);
        assert!(content.matches[0].file.ends_with("a.txt"));
    }

    #[test]
    fn content_mode_groups_matches_by_file() {
        let result = exec_grep_shape(
            "content",
            std::path::Path::new("/tmp"),
            "a.txt:2:hello\nb.txt:7:hello\na.txt:9:hello again\n",
        );
        let Some(proto::grep_union_result::Result::Content(content)) = result.result else {
            panic!("expected content result");
        };
        assert_eq!(content.total_matched_lines, 3);
        assert_eq!(content.matches.len(), 2);
        assert_eq!(content.matches[0].file, "/tmp/a.txt");
        assert_eq!(content.matches[0].matches.len(), 2);
        assert_eq!(content.matches[1].matches[0].line_number, 7);
    }

    #[test]
    fn count_mode_sums_per_file_counts() {
        let result = exec_grep_shape("count", std::path::Path::new("/tmp"), "a.txt:4\nb.txt:6\n");
        let Some(proto::grep_union_result::Result::Count(count)) = result.result else {
            panic!("expected count result");
        };
        assert_eq!(count.total_files, 2);
        assert_eq!(count.total_matches, 10);
    }

    #[test]
    fn files_mode_lists_paths() {
        let result = exec_grep_shape(
            "files_with_matches",
            std::path::Path::new("/tmp"),
            "a.txt\nb.txt\n",
        );
        let Some(proto::grep_union_result::Result::Files(files)) = result.result else {
            panic!("expected files result");
        };
        assert_eq!(files.files, vec!["a.txt".to_string(), "b.txt".to_string()]);
        assert_eq!(files.total_files, 2);
    }
}

#[cfg(test)]
mod exec_shell_tests {
    use super::*;

    fn args(command: &str, timeout: i32) -> proto::ShellArgs {
        proto::ShellArgs {
            command: command.to_string(),
            working_directory: String::new(),
            timeout,
        }
    }

    #[test]
    fn output_is_reported_while_the_command_runs() {
        let mut chunks = Vec::new();
        let (stdout, _, code, _) = exec_shell_run(&args("printf one; printf two", 10), &mut |_, text| {
            chunks.push(text.to_string())
        });
        assert_eq!(code, 0);
        assert_eq!(stdout, "onetwo");
        assert_eq!(chunks.concat(), "onetwo");
    }

    #[test]
    fn a_hung_command_is_killed_at_the_deadline_without_external_timeout() {
        let started = std::time::Instant::now();
        let (_, _, code, _) = exec_shell_run(&args("sleep 30", 1), &mut |_, _| {});
        assert_eq!(code, -1, "a killed command has no exit status");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn truncation_is_announced_to_the_model() {
        let (stdout, stderr, code, _) = exec_shell_run(
            &args("head -c 250000 /dev/zero | tr '\\0' 'a'", 30),
            &mut |_, _| {},
        );
        assert_eq!(code, 0);
        assert!(stdout.len() <= EXEC_OUTPUT_LIMIT);
        assert!(stderr.contains("truncated at 200000 bytes"));
    }
}

#[cfg(test)]
mod exec_reply_tests {
    use super::*;

    #[test]
    fn unsupported_exec_request_throws_and_closes_instead_of_hanging() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let mut decoder = CursorDecoder::with_reply_sender(tx);
        let mut out = Vec::new();
        decoder.reply_to_exec(
            proto::ExecServerMessage {
                id: 3,
                exec_id: "e-3".to_string(),
                message: None,
            },
            &mut out,
        );
        assert!(
            out.is_empty(),
            "unsupported exec must not kill the turn: {out:?}"
        );
        let decode = |frame: bytes::Bytes| {
            proto::AgentClientMessage::decode(&frame[5..]).expect("reply frame must decode")
        };
        let throw = decode(rx.try_recv().expect("throw frame"));
        match throw.message {
            Some(proto::agent_client_message::Message::ExecClientControlMessage(control)) => {
                match control.message {
                    Some(proto::exec_client_control_message::Message::Throw(thrown)) => {
                        assert_eq!(thrown.id, 3);
                        assert!(!thrown.error.is_empty());
                    }
                    other => panic!("expected a throw, got {other:?}"),
                }
            }
            other => panic!("expected a control message, got {other:?}"),
        }
        let close = decode(rx.try_recv().expect("stream close frame"));
        match close.message {
            Some(proto::agent_client_message::Message::ExecClientControlMessage(control)) => {
                match control.message {
                    Some(proto::exec_client_control_message::Message::StreamClose(closed)) => {
                        assert_eq!(closed.id, 3);
                    }
                    other => panic!("expected a stream close, got {other:?}"),
                }
            }
            other => panic!("expected a control message, got {other:?}"),
        }
    }
}

const EXEC_OUTPUT_LIMIT: usize = 200_000;

pub(crate) fn exec_root() -> std::path::PathBuf {
    std::env::var("MAHOQUOT_EXEC_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/"))
}

pub(crate) fn exec_resolve(path: &str) -> Result<std::path::PathBuf, String> {
    let root = exec_root();
    let candidate = std::path::Path::new(path);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    };
    let normalized = joined.canonicalize().unwrap_or(joined);
    if root != std::path::Path::new("/") {
        let root = root.canonicalize().unwrap_or(root);
        if !normalized.starts_with(&root) {
            return Err(format!(
                "path escapes the configured exec root {}",
                root.display()
            ));
        }
    }
    Ok(normalized)
}

pub(crate) fn exec_truncate(text: &mut String) -> bool {
    if text.len() <= EXEC_OUTPUT_LIMIT {
        return false;
    }
    let mut boundary = EXEC_OUTPUT_LIMIT;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    true
}

pub(crate) fn exec_request_context() -> proto::exec_client_message::Message {
    let context = proto::RequestContext {
        env: Some(proto::RequestContextEnv {
            os_version: std::env::consts::OS.to_string(),
            workspace_paths: vec![exec_root().display().to_string()],
            shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string()),
            sandbox_enabled: false,
            time_zone: "UTC".to_string(),
        }),
        tools: Vec::new(),
    };
    proto::exec_client_message::Message::RequestContextResult(proto::RequestContextResult {
        result: Some(proto::request_context_result::Result::Success(
            proto::RequestContextSuccess {
                request_context: Some(context),
                served_from_disk_cache: Some(false),
            },
        )),
    })
}

pub(crate) fn exec_read(args: &proto::ReadArgs) -> proto::exec_client_message::Message {
    use proto::read_result::Result as ReadOutcome;
    let result = match exec_resolve(&args.path) {
        Err(error) => ReadOutcome::Error(proto::ReadFailure {
            path: args.path.clone(),
            error,
        }),
        Ok(path) => match std::fs::read_to_string(&path) {
            Ok(mut content) => {
                let file_size = content.len() as i64;
                let truncated = exec_truncate(&mut content);
                let total_lines = content.lines().count() as i32;
                ReadOutcome::Success(proto::ReadSuccess {
                    path: path.display().to_string(),
                    content: Some(content),
                    total_lines,
                    file_size,
                    truncated,
                })
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ReadOutcome::FileNotFound(proto::ReadFileNotFound {
                    path: path.display().to_string(),
                })
            }
            Err(error) => ReadOutcome::Error(proto::ReadFailure {
                path: path.display().to_string(),
                error: error.to_string(),
            }),
        },
    };
    proto::exec_client_message::Message::ReadResult(proto::ReadResult {
        result: Some(result),
    })
}

pub(crate) fn exec_write(args: &proto::WriteArgs) -> proto::exec_client_message::Message {
    let result = match exec_resolve(&args.path) {
        Err(error) => proto::write_result::Result::Error(proto::ReadFailure {
            path: args.path.clone(),
            error,
        }),
        Ok(path) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let bytes: &[u8] = if args.file_bytes.is_empty() {
                args.file_text.as_bytes()
            } else {
                args.file_bytes.as_slice()
            };
            match std::fs::write(&path, bytes) {
                Ok(()) => {
                    let text = String::from_utf8_lossy(bytes);
                    proto::write_result::Result::Success(proto::WriteSuccess {
                        path: path.display().to_string(),
                        lines_created: text.lines().count() as i32,
                        file_size: bytes.len() as i32,
                        file_content_after_write: args
                            .return_file_content_after_write
                            .then(|| text.to_string()),
                    })
                }
                Err(error) => {
                    if std::env::var_os("MAHOQUOT_EXEC_TRACE").is_some() {
                        eprintln!(
                            "mahoquot exec trace: write failed path={} err={error}",
                            path.display()
                        );
                    }
                    proto::write_result::Result::Error(proto::ReadFailure {
                        path: path.display().to_string(),
                        error: error.to_string(),
                    })
                }
            }
        }
    };
    proto::exec_client_message::Message::WriteResult(proto::WriteResult {
        result: Some(result),
    })
}

pub(crate) fn exec_delete(args: &proto::DeleteArgs) -> proto::exec_client_message::Message {
    let result = match exec_resolve(&args.path) {
        Err(error) => proto::delete_result::Result::Error(proto::ReadFailure {
            path: args.path.clone(),
            error,
        }),
        Ok(path) => match std::fs::metadata(&path) {
            Ok(meta) if !meta.is_file() => {
                proto::delete_result::Result::Error(proto::ReadFailure {
                    path: path.display().to_string(),
                    error: "not a regular file".to_string(),
                })
            }
            Ok(meta) => match std::fs::remove_file(&path) {
                Ok(()) => proto::delete_result::Result::Success(proto::DeleteSuccess {
                    deleted_file: path.display().to_string(),
                    path: path.display().to_string(),
                    file_size: meta.len() as i64,
                }),
                Err(error) => proto::delete_result::Result::Error(proto::ReadFailure {
                    path: path.display().to_string(),
                    error: error.to_string(),
                }),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                proto::delete_result::Result::FileNotFound(proto::ReadFileNotFound {
                    path: path.display().to_string(),
                })
            }
            Err(error) => proto::delete_result::Result::Error(proto::ReadFailure {
                path: path.display().to_string(),
                error: error.to_string(),
            }),
        },
    };
    proto::exec_client_message::Message::DeleteResult(proto::DeleteResult {
        result: Some(result),
    })
}

pub(crate) fn exec_ls(args: &proto::LsArgs) -> proto::exec_client_message::Message {
    let target = if args.path.trim().is_empty() {
        exec_root()
    } else {
        match exec_resolve(&args.path) {
            Ok(path) => path,
            Err(error) => {
                return proto::exec_client_message::Message::LsResult(proto::LsResult {
                    result: Some(proto::ls_result::Result::Error(proto::ReadFailure {
                        path: args.path.clone(),
                        error,
                    })),
                })
            }
        }
    };
    match std::fs::read_dir(&target) {
        Ok(entries) => {
            let num_files = entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| {
                    entry
                        .file_type()
                        .map(|kind| kind.is_file())
                        .unwrap_or(false)
                })
                .count() as i32;
            proto::exec_client_message::Message::LsResult(proto::LsResult {
                result: Some(proto::ls_result::Result::Success(proto::LsSuccess {
                    directory_tree_root: Some(proto::LsDirectoryTreeNode {
                        abs_path: target.display().to_string(),
                        num_files,
                    }),
                })),
            })
        }
        Err(error) => proto::exec_client_message::Message::LsResult(proto::LsResult {
            result: Some(proto::ls_result::Result::Error(proto::ReadFailure {
                path: target.display().to_string(),
                error: error.to_string(),
            })),
        }),
    }
}

pub(crate) fn exec_grep(args: &proto::GrepArgs) -> proto::exec_client_message::Message {
    match exec_grep_search(args) {
        Ok(result) => proto::exec_client_message::Message::GrepResult(proto::GrepResult {
            result: Some(proto::grep_result::Result::Success(proto::GrepSuccess {
                pattern: args.pattern.clone(),
                path: args.path.clone().unwrap_or_default(),
                output_mode: exec_grep_mode(args),
                workspace_results: std::collections::HashMap::from([(
                    args.path
                        .clone()
                        .filter(|path| !path.trim().is_empty())
                        .unwrap_or_else(|| exec_root().display().to_string()),
                    result,
                )]),
                active_editor_result: None,
            })),
        }),
        Err(error) => proto::exec_client_message::Message::GrepResult(proto::GrepResult {
            result: Some(proto::grep_result::Result::Error(proto::GrepFailure { error })),
        }),
    }
}

struct ExecSearchTool {
    binary: std::path::PathBuf,
    prefix: Vec<String>,
}

fn exec_search_tool() -> ExecSearchTool {
    let mut candidates: Vec<std::path::PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(std::env::split_paths(&path).map(|dir| dir.join("rg")));
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::PathBuf::from(home);
        candidates.push(home.join(".omo/agent/bin/rg"));
        candidates.push(home.join(".local/bin/rg"));
        candidates.push(home.join(".cargo/bin/rg"));
    }
    candidates.push(std::path::PathBuf::from("/opt/homebrew/bin/rg"));
    candidates.push(std::path::PathBuf::from("/usr/local/bin/rg"));
    for candidate in candidates {
        if candidate.is_file() {
            return ExecSearchTool {
                binary: candidate,
                prefix: Vec::new(),
            };
        }
    }
    ExecSearchTool {
        binary: std::path::PathBuf::from("grep"),
        prefix: vec![
            "-R".to_string(),
            "--line-number".to_string(),
            "--binary-files=without-match".to_string(),
            "--exclude-dir=.git".to_string(),
            "--exclude-dir=node_modules".to_string(),
        ],
    }
}

fn exec_grep_search(
    args: &proto::GrepArgs,
) -> Result<proto::GrepUnionResult, String> {
    let root = match args.path.as_deref().filter(|path| !path.trim().is_empty()) {
        Some(path) => exec_resolve(path)?,
        None => exec_root(),
    };
    let mode = exec_grep_mode(args);
    let search = exec_search_tool();
    let mut command = std::process::Command::new(&search.binary);
    for flag in &search.prefix {
        command.arg(flag);
    }
    if search.prefix.is_empty() {
        command
            .arg("--no-heading")
            .arg("--line-number")
            .arg("--color=never");
    }
    let is_ripgrep = search.prefix.is_empty();
    match mode.as_str() {
        "files_with_matches" => {
            command.arg(if is_ripgrep {
                "--files-with-matches"
            } else {
                "-l"
            });
        }
        "count" => {
            command.arg("--count");
        }
        _ => {}
    }
    if args.case_insensitive.unwrap_or(false) {
        command.arg("--ignore-case");
    }
    if let Some(glob) = args.glob.as_deref().filter(|glob| !glob.trim().is_empty()) {
        if is_ripgrep {
            command.arg("--glob").arg(glob);
        } else {
            command.arg(format!("--include={glob}"));
        }
    }
    let context = args.context.unwrap_or(0);
    let before = args.context_before.unwrap_or(context);
    let after = args.context_after.unwrap_or(context);
    if before > 0 {
        command.arg("--before-context").arg(before.to_string());
    }
    if after > 0 {
        command.arg("--after-context").arg(after.to_string());
    }
    command
        .arg("--")
        .arg(&args.pattern)
        .arg(&root)
        .current_dir(&root);
    let output = command
        .output()
        .map_err(|error| format!("failed to run rg: {error}"))?;
    if !output.status.success() && output.status.code() != Some(1) {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    Ok(exec_grep_shape(&mode, &root, &text))
}

fn exec_grep_mode(args: &proto::GrepArgs) -> String {
    match args.output_mode.as_deref() {
        Some("files") | Some("files_with_matches") => "files_with_matches".to_string(),
        Some("count") => "count".to_string(),
        _ => "content".to_string(),
    }
}

fn exec_grep_shape(mode: &str, root: &std::path::Path, text: &str) -> proto::GrepUnionResult {
    use proto::grep_union_result::Result as Shape;
    let shape = match mode {
        "files_with_matches" => {
            let files: Vec<String> = text.lines().map(|line| line.to_string()).collect();
            Shape::Files(proto::GrepFilesResult {
                total_files: files.len() as i32,
                files,
                client_truncated: false,
                ripgrep_truncated: false,
            })
        }
        "count" => {
            let mut counts = Vec::new();
            let mut total = 0;
            for line in text.lines() {
                if let Some((file, count)) = line.rsplit_once(':') {
                    if let Ok(count) = count.parse::<i32>() {
                        total += count;
                        counts.push(proto::GrepFileCount {
                            file: absolute(root, file),
                            count,
                        });
                    }
                }
            }
            Shape::Count(proto::GrepCountResult {
                total_files: counts.len() as i32,
                total_matches: total,
                counts,
                client_truncated: false,
                ripgrep_truncated: false,
            })
        }
        _ => {
            let mut grouped: Vec<proto::GrepFileMatch> = Vec::new();
            let mut matched = 0;
            for line in text.lines() {
                let Some((file, rest)) = line.split_once(':') else {
                    continue;
                };
                let Some((number, content)) = rest.split_once(':') else {
                    continue;
                };
                let Ok(line_number) = number.parse::<i32>() else {
                    continue;
                };
                let path = absolute(root, file);
                let entry = proto::GrepContentMatch {
                    line_number,
                    content: content.to_string(),
                    content_truncated: false,
                    is_context_line: false,
                };
                matched += 1;
                match grouped.iter_mut().find(|group| group.file == path) {
                    Some(group) => group.matches.push(entry),
                    None => grouped.push(proto::GrepFileMatch {
                        file: path,
                        matches: vec![entry],
                    }),
                }
            }
            Shape::Content(proto::GrepContentResult {
                total_lines: grouped.len() as i32,
                total_matched_lines: matched,
                matches: grouped,
                client_truncated: false,
                ripgrep_truncated: false,
            })
        }
    };
    proto::GrepUnionResult {
        result: Some(shape),
    }
}

fn absolute(root: &std::path::Path, file: &str) -> String {
    if std::path::Path::new(file).is_absolute() {
        file.to_string()
    } else {
        root.join(file).display().to_string()
    }
}

pub(crate) fn exec_shell_capture(args: &proto::ShellArgs) -> (String, String, i32, String) {
    exec_shell_run(args, &mut |_, _| {})
}

/// Runs a shell command with a hard deadline and reports output as it arrives, so a long
/// command shows progress instead of appearing frozen until it exits.
pub(crate) fn exec_shell_run(
    args: &proto::ShellArgs,
    on_chunk: &mut dyn FnMut(bool, &str),
) -> (String, String, i32, String) {
    use std::io::Read;
    use std::sync::mpsc;

    let directory = if args.working_directory.trim().is_empty() {
        exec_root()
    } else {
        match exec_resolve(&args.working_directory) {
            Ok(path) => path,
            Err(error) => return (String::new(), error, -1, args.working_directory.clone()),
        }
    };
    let timeout = if args.timeout > 0 {
        args.timeout.clamp(1, 600)
    } else {
        120
    };
    let spawned = std::process::Command::new("sh")
        .arg("-c")
        .arg(&args.command)
        .current_dir(&directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            return (
                String::new(),
                error.to_string(),
                -1,
                directory.display().to_string(),
            )
        }
    };

    let (tx, rx) = mpsc::channel::<(bool, Vec<u8>)>();
    fn pump<R: Read + Send + 'static>(
        mut reader: R,
        is_err: bool,
        tx: mpsc::Sender<(bool, Vec<u8>)>,
    ) {
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if tx.send((is_err, buffer[..read].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });
    }
    if let Some(stdout) = child.stdout.take() {
        pump(stdout, false, tx.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        pump(stderr, true, tx.clone());
    }
    drop(tx);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout as u64);
    let mut stdout_raw = Vec::new();
    let mut stderr_raw = Vec::new();
    let mut stdout_carry = Vec::new();
    let mut stderr_carry = Vec::new();
    let mut open = 2;
    while open > 0 {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            let _ = child.kill();
            break;
        }
        match rx.recv_timeout(remaining.min(std::time::Duration::from_millis(250))) {
            Ok((is_err, chunk)) => {
                let (raw, carry) = if is_err {
                    (&mut stderr_raw, &mut stderr_carry)
                } else {
                    (&mut stdout_raw, &mut stdout_carry)
                };
                raw.extend_from_slice(&chunk);
                carry.extend_from_slice(&chunk);
                let text = take_utf8(carry);
                if !text.is_empty() {
                    on_chunk(is_err, &text);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => open -= 1,
        }
    }
    let status = child.wait().ok();
    for (is_err, carry) in [(false, stdout_carry), (true, stderr_carry)] {
        if !carry.is_empty() {
            on_chunk(is_err, &String::from_utf8_lossy(&carry));
        }
    }
    let mut stdout = String::from_utf8_lossy(&stdout_raw).to_string();
    let mut stderr = String::from_utf8_lossy(&stderr_raw).to_string();
    if exec_truncate(&mut stdout) {
        stderr.push_str("\n[stdout truncated at 200000 bytes by the gateway executor]");
    }
    if exec_truncate(&mut stderr) {
        stderr.push_str("\n[stderr truncated at 200000 bytes by the gateway executor]");
    }
    let code = status.and_then(|status| status.code()).unwrap_or(-1);
    (stdout, stderr, code, directory.display().to_string())
}

fn take_utf8(carry: &mut Vec<u8>) -> String {
    match std::str::from_utf8(carry) {
        Ok(text) => {
            let owned = text.to_string();
            carry.clear();
            owned
        }
        Err(error) => {
            let valid = error.valid_up_to();
            if valid == 0 {
                return String::new();
            }
            let owned = String::from_utf8_lossy(&carry[..valid]).to_string();
            carry.drain(..valid);
            owned
        }
    }
}

pub(crate) fn shell_result_message(
    command: &str,
    cwd: String,
    exit_code: i32,
    stdout: String,
    stderr: String,
    execution_time: i32,
) -> proto::exec_client_message::Message {
    let payload = proto::ShellSuccess {
        command: command.to_string(),
        working_directory: cwd,
        exit_code,
        signal: if exit_code == -1 {
            "killed".to_string()
        } else {
            String::new()
        },
        stdout,
        stderr,
        execution_time,
    };
    let outcome = if exit_code == 0 {
        proto::shell_result::Result::Success(payload)
    } else {
        proto::shell_result::Result::Failure(payload)
    };
    proto::exec_client_message::Message::ShellResult(proto::ShellResult {
        result: Some(outcome),
    })
}

pub(crate) fn exec_shell(args: &proto::ShellArgs) -> proto::exec_client_message::Message {
    let started = std::time::Instant::now();
    let (stdout, stderr, exit_code, cwd) = exec_shell_capture(args);
    let payload = proto::ShellSuccess {
        command: args.command.clone(),
        working_directory: cwd,
        exit_code,
        signal: if exit_code == -1 {
            "killed".to_string()
        } else {
            String::new()
        },
        stdout,
        stderr,
        execution_time: started.elapsed().as_millis() as i32,
    };
    let outcome = if exit_code == 0 {
        proto::shell_result::Result::Success(payload)
    } else {
        proto::shell_result::Result::Failure(payload)
    };
    proto::exec_client_message::Message::ShellResult(proto::ShellResult {
        result: Some(outcome),
    })
}

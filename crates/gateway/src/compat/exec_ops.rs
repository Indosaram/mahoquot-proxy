//! Cursor agent exec handling, shared by the gateway and the local exec sidecar.
//!
//! Cursor does not run tools on its own host: it asks its client (this gateway) over the
//! bidi stream. The reply frames are built here so that a gateway running on another
//! machine can delegate them to a sidecar that lives next to the user's files.

use super::cursor::{
    exec_delete, exec_grep, exec_ls, exec_read, exec_request_context,
    exec_shell, exec_shell_run, exec_trace_enabled, exec_write, shell_result_message,
};
use super::cursor_proto as proto;

pub fn unsupported(id: u32, exec_id: &str, error: &str) -> Vec<proto::AgentClientMessage> {
    let frame = |message: proto::exec_client_control_message::Message| proto::AgentClientMessage {
        message: Some(proto::agent_client_message::Message::ExecClientControlMessage(
            proto::ExecClientControlMessage {
                message: Some(message),
            },
        )),
    };
    let _ = exec_id;
    vec![
        frame(proto::exec_client_control_message::Message::Throw(
            proto::ExecClientThrow {
                id,
                error: error.to_string(),
            },
        )),
        frame(proto::exec_client_control_message::Message::StreamClose(
            proto::ExecClientStreamClose { id },
        )),
    ]
}

pub fn handle(message: &proto::ExecServerMessage) -> Vec<proto::AgentClientMessage> {
    let mut frames = Vec::new();
    handle_with(message, &mut |frame| frames.push(frame));
    frames
}

/// Emits each reply frame as soon as it exists, so a long command reports progress
/// instead of one silent block at the end.
pub fn handle_with(
    message: &proto::ExecServerMessage,
    emit: &mut dyn FnMut(proto::AgentClientMessage),
) {
    use proto::exec_server_message::Message as Req;

    let id = message.id;
    let exec_id = message.exec_id.clone();
    // exec trace: shows which host handled a request and which variant it was.
    let variant = match &message.message {
        Some(Req::ShellArgs(_)) => "shell",
        Some(Req::ShellStreamArgs(_)) => "shell_stream",
        Some(Req::WriteArgs(_)) => "write",
        Some(Req::DeleteArgs(_)) => "delete",
        Some(Req::GrepArgs(_)) => "grep",
        Some(Req::ReadArgs(_)) => "read",
        Some(Req::LsArgs(_)) => "ls",
        Some(_) => "other",
        None => "none",
    };
    if exec_trace_enabled() {
        eprintln!(
            "mahoquot exec trace: variant={variant} id={id} server={}",
            std::env::var("MAHOQUOT_EXEC_SERVER").unwrap_or_else(|_| "gateway".to_string())
        );
    }
    let exec = |payload: proto::exec_client_message::Message| proto::AgentClientMessage {
        message: Some(proto::agent_client_message::Message::ExecClientMessage(
            proto::ExecClientMessage {
                id,
                exec_id: exec_id.clone(),
                message: Some(payload),
            },
        )),
    };
    let control =
        |message: proto::exec_client_control_message::Message| proto::AgentClientMessage {
            message: Some(
                proto::agent_client_message::Message::ExecClientControlMessage(
                    proto::ExecClientControlMessage {
                        message: Some(message),
                    },
                ),
            ),
        };
    if let Some(Req::ShellStreamArgs(args)) = &message.message {
        let started = std::time::Instant::now();
        emit(exec(proto::exec_client_message::Message::ShellStream(
            proto::ShellStream {
                event: Some(proto::shell_stream::Event::Start(
                    proto::ShellStreamStart {},
                )),
            },
        )));
        let mut pending: Vec<(bool, String)> = Vec::new();
        let flush = |pending: &mut Vec<(bool, String)>,
                     emit: &mut dyn FnMut(proto::AgentClientMessage)| {
            for (is_err, data) in std::mem::take(pending) {
                let event = if is_err {
                    proto::shell_stream::Event::Stderr(proto::ShellStreamStderr { data })
                } else {
                    proto::shell_stream::Event::Stdout(proto::ShellStreamStdout { data })
                };
                emit(exec(proto::exec_client_message::Message::ShellStream(
                    proto::ShellStream { event: Some(event) },
                )));
            }
        };
        let (stdout, stderr, code, cwd) = exec_shell_run(args, &mut |is_err, text| {
            match pending.last_mut() {
                Some((was_err, buffer)) if *was_err == is_err && buffer.len() < 8192 => {
                    buffer.push_str(text);
                    return;
                }
                _ => {}
            }
            pending.push((is_err, text.to_string()));
            if pending.len() >= 4 {
                flush(&mut pending, emit);
            }
        });
        flush(&mut pending, emit);
        emit(exec(proto::exec_client_message::Message::ShellStream(
            proto::ShellStream {
                event: Some(proto::shell_stream::Event::Exit(proto::ShellStreamExit {
                    code: code.max(0) as u32,
                    cwd: cwd.clone(),
                    aborted: false,
                })),
            },
        )));
        // Cursor keeps the turn pending unless the structured result follows the stream
        // events and the exec stream is then closed.
        emit(exec(shell_result_message(
            &args.command,
            cwd,
            code,
            stdout,
            stderr,
            started.elapsed().as_millis() as i32,
        )));
        emit(control(
            proto::exec_client_control_message::Message::StreamClose(
                proto::ExecClientStreamClose { id },
            ),
        ));
        return;
    }

    let reply = match message.message {
        Some(Req::RequestContextArgs(_)) => Some(exec_request_context()),
        Some(Req::ReadArgs(ref args)) => Some(exec_read(args)),
        Some(Req::WriteArgs(ref args)) => Some(exec_write(args)),
        Some(Req::DeleteArgs(ref args)) => Some(exec_delete(args)),
        Some(Req::LsArgs(ref args)) => Some(exec_ls(args)),
        Some(Req::GrepArgs(ref args)) => Some(exec_grep(args)),
        Some(Req::ShellArgs(ref args)) => Some(exec_shell(args)),
        Some(Req::DiagnosticsArgs(_)) => {
            Some(proto::exec_client_message::Message::DiagnosticsResult(
                proto::DiagnosticsResult::default(),
            ))
        }
        Some(Req::ListMcpResourcesExecArgs(_)) => Some(
            proto::exec_client_message::Message::ListMcpResourcesExecResult(
                proto::ListMcpResourcesExecResult {
                    result: Some(proto::list_mcp_resources_exec_result::Result::Success(
                        proto::ListMcpResourcesSuccess {
                            resources: Vec::new(),
                        },
                    )),
                },
            ),
        ),
        Some(Req::ReadMcpResourceExecArgs(_)) => Some(
            proto::exec_client_message::Message::ReadMcpResourceExecResult(
                proto::ReadMcpResourceExecResult {
                    result: Some(proto::read_mcp_resource_exec_result::Result::Error(
                        proto::ReadMcpResourceError {
                            error: "MCP resources are not exposed by this gateway".to_string(),
                        },
                    )),
                },
            ),
        ),
        _ => None,
    };

    match reply {
        Some(payload) => emit(exec(payload)),
        // Anything we cannot model must still be answered, otherwise the server-side agent
        // waits forever. A throw plus a stream close is a normal tool failure to Cursor.
        None => {
            for frame in unsupported(id, &exec_id, "exec request is not supported by this executor")
            {
                emit(frame);
            }
        }
    }
}

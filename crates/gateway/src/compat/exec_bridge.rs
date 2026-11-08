//! Delegates Cursor exec requests to a sidecar on the user's machine.
//!
//! A gateway running on another host cannot touch the user's files, so when
//! `MAHOQUOT_EXEC_BRIDGE` names a sidecar the exec frames are forwarded there and the
//! returned reply frames are relayed back unchanged.
//!
//! The sidecar side is a small HTTP server (`MAHOQUOT_EXEC_SERVER`) that lives next to the
//! user's files. It requires `MAHOQUOT_EXEC_TOKEN` when that variable is set on either end.

use std::io::{BufRead, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use prost::Message;

use super::cursor_proto as proto;
use std::net::ToSocketAddrs;

const BRIDGE_TIMEOUT: Duration = Duration::from_secs(630);
const BRIDGE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_BRIDGE_BODY: usize = 8 * 1024 * 1024;

pub fn target() -> Option<String> {
    std::env::var("MAHOQUOT_EXEC_BRIDGE")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn token() -> Option<String> {
    std::env::var("MAHOQUOT_EXEC_TOKEN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn address(raw: &str) -> Option<String> {
    let trimmed = raw
        .strip_prefix("http://")
        .or_else(|| raw.strip_prefix("https://"))
        .unwrap_or(raw);
    let trimmed = trimmed.trim_end_matches('/');
    if trimmed.contains(':') {
        Some(trimmed.to_string())
    } else {
        Some(format!("{trimmed}:18860"))
    }
}

pub fn forward(
    message: &proto::ExecServerMessage,
) -> Result<Option<Vec<proto::AgentClientMessage>>, String> {
    let mut frames = Vec::new();
    let forwarded = forward_stream(message, &mut |frame| frames.push(frame))?;
    Ok(forwarded.then_some(frames))
}

/// Relays each reply frame as soon as the sidecar sends it. Returns whether a bridge was
/// configured; a bridge that is configured but unreachable is an error, never a fallback.
pub fn forward_stream(
    message: &proto::ExecServerMessage,
    emit: &mut dyn FnMut(proto::AgentClientMessage),
) -> Result<bool, String> {
    let Some(raw) = target() else {
        // Standalone gateways execute exec requests on their own host. Say so once instead of
        // letting an operator believe the frames reached the user's machine.
        static WARNED: std::sync::Once = std::sync::Once::new();
        WARNED.call_once(|| {
            eprintln!(
                "mahoquot: MAHOQUOT_EXEC_BRIDGE is unset, so exec requests run on the gateway host"
            );
        });
        return Ok(false);
    };
    forward_to(&raw, message, emit)?;
    Ok(true)
}

fn forward_to(
    raw: &str,
    message: &proto::ExecServerMessage,
    emit: &mut dyn FnMut(proto::AgentClientMessage),
) -> Result<(), String> {
    let address = address(raw).ok_or_else(|| format!("invalid exec bridge {raw:?}"))?;
    let payload = message.encode_to_vec();
    let connect = address.as_str().to_socket_addrs()
        .map_err(|error| format!("exec sidecar {address} is unresolvable: {error}"))?
        .next()
        .ok_or_else(|| format!("exec sidecar {address} resolved to no address"))?;
    let mut stream = TcpStream::connect_timeout(&connect, BRIDGE_CONNECT_TIMEOUT)
        .map_err(|error| format!("exec sidecar {address} is unreachable: {error}"))?;
    stream
        .set_read_timeout(Some(BRIDGE_TIMEOUT))
        .map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|error| error.to_string())?;
    let auth = match token() {
        Some(token) => format!("X-Mahoquot-Exec-Token: {token}\r\n"),
        None => String::new(),
    };
    let head = format!(
        "POST /exec HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/octet-stream\r\n{auth}Content-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream
        .write_all(head.as_bytes())
        .map_err(|error| error.to_string())?;
    stream
        .write_all(&payload)
        .map_err(|error| error.to_string())?;

    let mut reader = std::io::BufReader::new(stream);
    let mut head = String::new();
    let mut chunked = false;
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .map_err(|error| format!("exec sidecar {address} failed mid-reply: {error}"))?
            == 0
        {
            return Err(format!("exec sidecar {address} closed before replying"));
        }
        if line == "\r\n" {
            break;
        }
        if head.is_empty() && !line.contains(" 200") {
            return Err(format!("exec sidecar {address} returned a non-200 response"));
        }
        if line.to_ascii_lowercase().starts_with("transfer-encoding:")
            && line.to_ascii_lowercase().contains("chunked")
        {
            chunked = true;
        }
        head.push_str(&line);
    }

    let mut emitted = 0usize;
    let mut body = FrameReader {
        reader,
        chunked,
        remaining: 0,
    };
    loop {
        let mut length = [0u8; 4];
        match body.read_exact(&mut length) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(format!("exec sidecar {address} failed mid-reply: {error}")),
        }
        let mut payload = vec![0u8; u32::from_be_bytes(length) as usize];
        body.read_exact(&mut payload)
            .map_err(|error| format!("exec sidecar {address} sent a truncated frame: {error}"))?;
        let frame = proto::AgentClientMessage::decode(payload.as_slice())
            .map_err(|error| format!("exec sidecar {address} sent an undecodable frame: {error}"))?;
        emit(frame);
        emitted += 1;
    }
    if emitted == 0 {
        return Err(format!("exec sidecar {address} returned no frames"));
    }
    Ok(())
}

struct FrameReader<R: BufRead> {
    reader: R,
    chunked: bool,
    remaining: usize,
}

impl<R: BufRead> FrameReader<R> {
    fn fill_chunk(&mut self) -> std::io::Result<()> {
        if !self.chunked || self.remaining > 0 {
            return Ok(());
        }
        let mut line = String::new();
        let size = loop {
            line.clear();
            if self.reader.read_line(&mut line)? == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "chunked body ended",
                ));
            }
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                break trimmed.to_string();
            }
        };
        let size = usize::from_str_radix(size.split(';').next().unwrap_or(&size).trim(), 16)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        self.remaining = size;
        Ok(())
    }
}

impl<R: BufRead> Read for FrameReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if !self.chunked {
            return self.reader.read(buffer);
        }
        self.fill_chunk()?;
        if self.remaining == 0 {
            return Ok(0);
        }
        let take = buffer.len().min(self.remaining);
        let read = self.reader.read(&mut buffer[..take])?;
        self.remaining -= read;
        if self.remaining == 0 {
            let mut trailer = String::new();
            let _ = self.reader.read_line(&mut trailer);
        }
        Ok(read)
    }
}

#[cfg(test)]
fn response_body(raw: &[u8]) -> Option<&[u8]> {
    let split = raw.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = std::str::from_utf8(&raw[..split]).ok()?;
    let status = head.lines().next()?;
    if !status.contains(" 200 ") && !status.ends_with(" 200") {
        return None;
    }
    Some(&raw[split + 4..])
}

#[cfg(test)]
fn decode_frames(body: &[u8]) -> Option<Vec<proto::AgentClientMessage>> {
    let mut frames = Vec::new();
    let mut offset = 0usize;
    while offset + 4 <= body.len() {
        let len = u32::from_be_bytes(body[offset..offset + 4].try_into().ok()?) as usize;
        offset += 4;
        let end = offset.checked_add(len)?;
        if end > body.len() {
            return None;
        }
        frames.push(proto::AgentClientMessage::decode(&body[offset..end]).ok()?);
        offset = end;
    }
    Some(frames)
}

pub async fn serve(address: &str) -> std::io::Result<()> {
    use axum::extract::DefaultBodyLimit;
    use axum::routing::post;
    use axum::Router;

    let app = Router::new()
        .route("/exec", post(handle))
        .layer(DefaultBodyLimit::max(MAX_BRIDGE_BODY));
    let listener = tokio::net::TcpListener::bind(address).await?;
    eprintln!("mahoquot exec sidecar listening on {address}");
    axum::serve(listener, app).await
}

async fn handle(
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::http::Response<axum::body::Body> {
    use axum::http::{StatusCode, header};

    let plain = |status: StatusCode| {
        axum::http::Response::builder()
            .status(status)
            .body(axum::body::Body::empty())
            .expect("static response builds")
    };

    if let Some(expected) = token() {
        let presented = headers
            .get("x-mahoquot-exec-token")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        if presented != expected {
            return plain(StatusCode::UNAUTHORIZED);
        }
    }

    let Ok(message) = proto::ExecServerMessage::decode(body.as_ref()) else {
        return plain(StatusCode::BAD_REQUEST);
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    std::thread::spawn(move || {
        super::exec_ops::handle_with(&message, &mut |frame| {
            let bytes = frame.encode_to_vec();
            let mut out = Vec::with_capacity(bytes.len() + 4);
            out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            out.extend_from_slice(&bytes);
            let _ = tx.blocking_send(out);
        });
    });

    let stream = futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|chunk| (Ok::<_, std::io::Error>(chunk), rx))
    });
    axum::http::Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .body(axum::body::Body::from_stream(stream))
        .expect("response builds")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_host_defaults_to_the_sidecar_port() {
        assert_eq!(address("127.0.0.1").as_deref(), Some("127.0.0.1:18860"));
        assert_eq!(
            address("http://100.78.73.127:18860/").as_deref(),
            Some("100.78.73.127:18860")
        );
    }

    #[test]
    fn frames_round_trip_through_the_wire_format() {
        let frame = proto::AgentClientMessage {
            message: Some(proto::agent_client_message::Message::ExecClientControlMessage(
                proto::ExecClientControlMessage {
                    message: Some(proto::exec_client_control_message::Message::StreamClose(
                        proto::ExecClientStreamClose { id: 9 },
                    )),
                },
            )),
        };
        let bytes = frame.encode_to_vec();
        let mut body = Vec::new();
        body.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        body.extend_from_slice(&bytes);
        let decoded = decode_frames(&body).expect("frames decode");
        assert_eq!(decoded, vec![frame]);
    }

    #[test]
    fn a_non_200_response_is_not_relayed() {
        assert!(response_body(b"HTTP/1.1 500 Oops\r\n\r\npayload").is_none());
    }

    #[test]
    fn an_unreachable_sidecar_is_reported_instead_of_falling_back() {
        let error = forward_to(
            "127.0.0.1:1",
            &proto::ExecServerMessage {
                id: 1,
                exec_id: "e-1".to_string(),
                message: None,
            },
            &mut |_| {},
        )
        .expect_err("an unreachable sidecar must not look like success");
        assert!(error.contains("unreachable"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_sidecar_greps_real_files() {
        use std::net::TcpListener;

        let probe = TcpListener::bind("127.0.0.1:0").expect("probe port");
        let port = probe.local_addr().expect("probe addr").port();
        drop(probe);
        let address = format!("127.0.0.1:{port}");
        let serving = address.clone();
        let server = tokio::spawn(async move {
            let _ = serve(&serving).await;
        });
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(&address).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let dir = std::env::temp_dir().join(format!("mahoquot-sidecar-grep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("notes.txt"), "alpha\nBETA_MARKER\ngamma\n").expect("write");
        let request = proto::ExecServerMessage {
            id: 7,
            exec_id: "e-7".to_string(),
            message: Some(proto::exec_server_message::Message::GrepArgs(
                proto::GrepArgs {
                    pattern: "BETA_MARKER".to_string(),
                    path: Some(dir.display().to_string()),
                    glob: None,
                    output_mode: Some("content".to_string()),
                    context_before: None,
                    context_after: None,
                    context: None,
                    case_insensitive: None,
                },
            )),
        };
        let result = tokio::task::spawn_blocking({
            let address = address.clone();
            move || {
                let mut frames = Vec::new();
                forward_to(&address, &request, &mut |frame| frames.push(frame))
                    .map(|()| frames)
            }
        })
        .await
        .expect("join");
        server.abort();
        let _ = std::fs::remove_dir_all(&dir);
        let frames = result.expect("sidecar greps");

        assert_eq!(frames.len(), 1);
        let Some(proto::agent_client_message::Message::ExecClientMessage(message)) =
            frames[0].message.as_ref()
        else {
            panic!("expected an exec reply");
        };
        let Some(proto::exec_client_message::Message::GrepResult(result)) = message.message.as_ref()
        else {
            panic!("expected a grep reply");
        };
        let Some(proto::grep_result::Result::Success(success)) = result.result.as_ref() else {
            panic!("grep must succeed: {result:?}");
        };
        let Some(proto::grep_union_result::Result::Content(content)) = success
            .workspace_results
            .values()
            .next()
            .and_then(|result| result.result.as_ref())
        else {
            panic!("expected content matches");
        };
        assert_eq!(content.total_matched_lines, 1);
        assert_eq!(content.matches[0].matches[0].line_number, 2);
        assert_eq!(content.matches[0].matches[0].content, "BETA_MARKER");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_sidecar_answers_with_frames() {
        use std::net::TcpListener;

        let probe = TcpListener::bind("127.0.0.1:0").expect("probe port");
        let port = probe.local_addr().expect("probe addr").port();
        drop(probe);
        let address = format!("127.0.0.1:{port}");
        let serving = address.clone();
        let server = tokio::spawn(async move {
            let _ = serve(&serving).await;
        });
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(&address).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }

        let request = proto::ExecServerMessage {
            id: 4,
            exec_id: "e-4".to_string(),
            message: Some(proto::exec_server_message::Message::ReadArgs(
                proto::ReadArgs {
                    path: "/etc/hostname".to_string(),
                },
            )),
        };
        let reply = tokio::task::spawn_blocking({
            let address = address.clone();
            move || {
                let mut frames = Vec::new();
                forward_to(&address, &request, &mut |frame| frames.push(frame))
                    .map(|()| frames)
            }
        })
        .await
        .expect("join")
        .expect("sidecar replies");
        server.abort();

        assert_eq!(reply.len(), 1);
        match reply[0].message.as_ref() {
            Some(proto::agent_client_message::Message::ExecClientMessage(message)) => {
                assert_eq!(message.id, 4);
                assert!(matches!(
                    message.message,
                    Some(proto::exec_client_message::Message::ReadResult(_))
                ));
            }
            other => panic!("expected an exec reply, got {other:?}"),
        }
    }
}

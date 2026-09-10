use mahoquot_gateway::compat::{
    self,
    events::{CodexEvent, SseParser},
    gemini::{gemini_json_to_openai, openai_to_gemini, GeminiDecoder},
};
use serde_json::json;
mod common;

#[test]
fn generated_call_ids_do_not_alias_provider_counter_ids() {
    for index in 0..32 {
        let generated = compat::signature_ledger::synthetic_call_id("review_collision");
        assert_ne!(generated, format!("call_review_collision_{index}"));
    }
}

#[tokio::test]
async fn http_terminal_variants_and_client_cancel_release_upstream() {
    use axum::{body::Body, response::Response, routing::post, Router};
    use bytes::Bytes;
    use futures::StreamExt;
    use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
    use std::{
        pin::Pin,
        sync::{Arc, Mutex},
        task::{Context, Poll},
    };
    struct FixtureBody {
        first: Option<Bytes>,
        dropped: Option<tokio::sync::oneshot::Sender<()>>,
    }
    impl futures::Stream for FixtureBody {
        type Item = Result<Bytes, std::io::Error>;
        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.first.take() {
                Some(first) => Poll::Ready(Some(Ok(first))),
                None => Poll::Pending,
            }
        }
    }
    impl Drop for FixtureBody {
        fn drop(&mut self) {
            if let Some(sender) = self.dropped.take() {
                let _ = sender.send(());
            }
        }
    }
    for terminal in [
        "response.completed",
        "response.failed",
        "response.incomplete",
        "response.cancelled",
        "client-drop",
    ] {
        let upstream_listener = listener().await;
        let upstream_url = format!("http://{}", upstream_listener.local_addr().unwrap());
        let (drop_tx, drop_rx) = tokio::sync::oneshot::channel();
        let drop_tx = Arc::new(Mutex::new(Some(drop_tx)));
        let upstream = Router::new().route(common::CODEX_PATH, post(move || {
            let sender = drop_tx.lock().unwrap().take().expect("no retry after committed response");
            async move {
                let mut wire = format!("data: {}\n\n", json!({"type":"response.output_text.delta","delta":"committed"}));
                if terminal != "client-drop" {
                    wire.push_str(&format!("data: {}\n\n", json!({"type":terminal,"response":{"status":"cancelled","error":{"message":"denied"},"incomplete_details":{"reason":"max_output_tokens"}}})));
                }
                Response::builder().header("content-type", "text/event-stream").body(Body::from_stream(FixtureBody {first:Some(Bytes::from(wire)), dropped:Some(sender)})).unwrap()
            }
        }));
        let dir = common::unique_temp_dir("pipeline-terminal-http");
        let mut cleanup = Cleanup {
            dir: dir.clone(),
            tasks: vec![tokio::spawn(async move {
                axum::serve(upstream_listener, upstream).await.unwrap();
            })],
        };
        let mut credential: serde_json::Value = serde_json::from_str(
            &common::create_auth_file_json("terminal", "fixture", "fixture", Some(&upstream_url)),
        )
        .unwrap();
        credential["usage_override"] = json!(upstream_url);
        std::fs::write(dir.join("codex-terminal.json"), credential.to_string()).unwrap();
        let config_path = dir.join("config.yaml");
        std::fs::write(&config_path, "logging-to-file: false\n").unwrap();
        let state = Arc::new(
            AppState::new(&GatewayConfig {
                auth_dir: dir.clone(),
                config_path,
                auth_refresh_enabled: false,
                ..Default::default()
            })
            .unwrap(),
        );
        let gateway_listener = listener().await;
        let gateway_url = format!("http://{}", gateway_listener.local_addr().unwrap());
        cleanup.tasks.push(tokio::spawn(async move {
            axum::serve(gateway_listener, create_app(state))
                .await
                .unwrap();
        }));
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(3))
            .build()
            .unwrap();
        let response = client.post(format!("{gateway_url}/v1/chat/completions")).json(&json!({"model":"gpt-5.6-sol","stream":true,"messages":[{"role":"user","content":"go"}]})).send().await.unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        if terminal == "client-drop" {
            let mut stream = response.bytes_stream();
            let mut received = Vec::new();
            while !received.windows(9).any(|w| w == b"committed") {
                received.extend_from_slice(&stream.next().await.unwrap().unwrap());
            }
            drop(stream);
        } else {
            let text = response
                .text()
                .await
                .expect("terminal must close HTTP body before upstream EOF");
            assert!(text.contains("committed"));
            assert_eq!(text.matches("[DONE]").count(), 1);
            if terminal != "response.completed" {
                assert!(text.contains("denied"));
                assert!(!text.contains("\"finish_reason\":\"stop\""));
            }
        }
        tokio::time::timeout(std::time::Duration::from_secs(3), drop_rx)
            .await
            .expect("upstream body must be dropped")
            .unwrap();
        for task in &cleanup.tasks {
            task.abort();
        }
        for task in cleanup.tasks.drain(..) {
            assert!(task.await.unwrap_err().is_cancelled());
        }
    }
}

#[test]
fn aggregate_reasoning_and_native_gemini_ids_signatures_survive() {
    let wire = format!(
        "data: {}\n\n",
        json!({"response":{"candidates":[{"content":{"parts":[
        {"text":"think","thought":true},
        {"functionCall":{"id":"aggregate-native","name":"custom_lookup","args":{}},"thoughtSignature":"NATIVE-SIG"}
    ]},"finishReason":"STOP"}]}})
    );
    let chat = compat::aggregate(
        wire.as_bytes(),
        "fixture".into(),
        0,
        compat::Protocol::Antigravity,
        compat::ReplyShape::Chat,
    )
    .unwrap();
    assert_eq!(chat["choices"][0]["message"]["reasoning_content"], "think");
    let native = compat::aggregate(
        wire.as_bytes(),
        "fixture".into(),
        0,
        compat::Protocol::Antigravity,
        compat::ReplyShape::Gemini,
    )
    .unwrap();
    let parts = native["candidates"][0]["content"]["parts"]
        .as_array()
        .unwrap();
    assert_eq!(parts[0], json!({"text":"think","thought":true}));
    assert_eq!(parts[1]["functionCall"]["id"], "aggregate-native");
    assert_eq!(parts[1]["thoughtSignature"], "NATIVE-SIG");
}

#[tokio::test]
async fn terminal_stream_drops_upstream_without_waiting_for_eof() {
    use bytes::Bytes;
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    struct Pending(Option<tokio::sync::oneshot::Sender<()>>);
    impl futures::Stream for Pending {
        type Item = reqwest::Result<Bytes>;
        fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Pending
        }
    }
    impl Drop for Pending {
        fn drop(&mut self) {
            self.0.take().unwrap().send(()).unwrap();
        }
    }
    for terminal in [
        "response.completed",
        "response.failed",
        "response.incomplete",
        "response.cancelled",
    ] {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let body = compat::streaming_body(compat::StreamingBodyParams {
            first: Bytes::from(format!(
                "data: {}\n\n",
                json!({"type":terminal,"response":{"status":"cancelled","error":{"message":"denied"}}})
            )),
            upstream: Box::pin(Pending(Some(tx))),
            model: "fixture".into(),
            created: 0,
            include_usage: false,
            shape: compat::ReplyShape::Chat,
            session: compat::ProtocolSession {
                protocol: compat::Protocol::Codex,
                cursor_reply: None,
            },
            upstream_capture: None,
        });
        let raw = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            axum::body::to_bytes(body, 10000),
        )
        .await
        .expect("terminal must not wait for upstream EOF")
        .unwrap();
        rx.await.unwrap();
        let text = String::from_utf8(raw.to_vec()).unwrap();
        assert_eq!(text.matches("[DONE]").count(), 1);
        if terminal != "response.completed" {
            assert!(!text.contains("\"finish_reason\":\"stop\""));
        }
    }
}

async fn listener() -> tokio::net::TcpListener {
    for port in 18840..=18899 {
        match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => return listener,
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => continue,
            Err(err) => panic!("bind {port}: {err}"),
        }
    }
    panic!("fixture ports exhausted")
}

struct Cleanup {
    tasks: Vec<tokio::task::JoinHandle<()>>,
    dir: std::path::PathBuf,
}
impl Drop for Cleanup {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        std::fs::remove_dir_all(&self.dir).expect("remove fixture auth directory");
    }
}

#[tokio::test]
async fn http_codex_and_gemini_two_turn_tools_refresh_and_explicit_parity_errors() {
    use axum::{
        body::Bytes,
        http::{HeaderMap, StatusCode, Uri},
        routing::post,
        Router,
    };
    use mahoquot_gateway::{config::GatewayConfig, routes::create_app, state::AppState};
    use serde_json::Value;
    use std::sync::{Arc, Mutex};
    for google in [false, true] {
        let upstream_listener = listener().await;
        let upstream_url = format!("http://{}", upstream_listener.local_addr().unwrap());
        let (captured, mut captures) = tokio::sync::mpsc::unbounded_channel();
        let phase = Arc::new(Mutex::new(0usize));
        let phase_handler = phase.clone();
        let upstream = Router::new().fallback(post(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let captured = captured.clone();
            let phase = phase_handler.clone();
            async move {
                captured.send((uri.path().to_string(), headers.clone(), body.clone())).unwrap();
                if uri.path() == "/token" {
                    return (StatusCode::OK, [("content-type", "application/json")], json!({"access_token":"fresh-token","refresh_token":"fresh-refresh","expires_in":3600}).to_string());
                }
                if uri.path().ends_with("/compact") {
                    return (StatusCode::OK, [("content-type", "application/json")], json!({"id":"compact-fixture","object":"response.compaction","output":[{"type":"compaction","encrypted_content":"opaque-fixture"}]}).to_string());
                }
                if !google && headers.get("authorization").unwrap() == "Bearer stale-token" {
                    return (StatusCode::UNAUTHORIZED, [("content-type", "application/json")], json!({"error":{"message":"expired"}}).to_string());
                }
                let mut phase = phase.lock().unwrap();
                *phase += 1;
                let wire = if google {
                    if *phase == 1 {
                        format!("data: {}\n\n", json!({"response":{"candidates":[{"content":{"parts":[
                            {"functionCall":{"id":"http-g-a","name":"custom_lookup","args":{"n":1}},"thoughtSignature":"HTTP-SIG"},
                            {"functionCall":{"id":"http-g-b","name":"edit","args":{"n":2}}}
                        ]},"finishReason":"STOP"}]}}))
                    } else { format!("data: {}\n\n", json!({"response":{"candidates":[{"content":{"parts":[{"text":"done"}]},"finishReason":"STOP"}]}})) }
                } else if *phase == 1 {
                    [json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"http-c-a","name":"custom_lookup"}}),
                     json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"n\":"}),
                     json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"1}"}),
                     json!({"type":"response.completed","response":{"status":"completed"}})]
                    .into_iter().map(|v| format!("data: {v}\n\n")).collect()
                } else { common::codex_sse("done") };
                (StatusCode::OK, [("content-type", "text/event-stream")], wire)
            }
        }));
        let dir = common::unique_temp_dir("review-pipeline-http");
        let mut cleanup = Cleanup {
            dir: dir.clone(),
            tasks: vec![tokio::spawn(async move {
                axum::serve(upstream_listener, upstream).await.unwrap();
            })],
        };
        let mut credential: Value = if google {
            json!({"type":"antigravity","identity_slug":"fixture-google","access_token":"fixture-token","refresh_token":"unused","project_id":"fixture-project","email":"fixture@example.test","expired":"2099-01-01T00:00:00Z"})
        } else {
            serde_json::from_str(&common::create_auth_file_json(
                "fixture-codex",
                "fixture-account",
                "stale-token",
                Some(&upstream_url),
            ))
            .unwrap()
        };
        credential["upstream_override"] = json!(upstream_url);
        credential["usage_override"] = json!(upstream_url);
        std::fs::write(
            dir.join(if google {
                "antigravity-fixture.json"
            } else {
                "codex-fixture.json"
            }),
            credential.to_string(),
        )
        .unwrap();
        let config_path = dir.join("config.yaml");
        std::fs::write(&config_path, "logging-to-file: false\n").unwrap();
        let state = Arc::new(
            AppState::new(&GatewayConfig {
                auth_dir: dir.clone(),
                config_path,
                refresh_url: format!("{upstream_url}/token"),
                auth_refresh_enabled: true,
                ..Default::default()
            })
            .unwrap(),
        );
        assert_eq!(state.pool.load().members.len(), 1);
        let gateway_listener = listener().await;
        let gateway_url = format!("http://{}", gateway_listener.local_addr().unwrap());
        cleanup.tasks.push(tokio::spawn(async move {
            axum::serve(gateway_listener, create_app(state))
                .await
                .unwrap();
        }));
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap();
        let model = if google {
            "gemini-3-flash"
        } else {
            "gpt-5.6-sol"
        };
        let mut request = json!({"model":model,"stream":false,"messages":[{"role":"user","content":"go"}],"tools":[{"type":"function","function":{"name":"custom_lookup","parameters":{"type":"object"}}},{"type":"function","function":{"name":"edit","parameters":{"type":"object"}}}]});
        let reply = client
            .post(format!("{gateway_url}/v1/chat/completions"))
            .json(&request)
            .send()
            .await
            .unwrap();
        let status = reply.status();
        let reply: Value = reply.json().await.unwrap();
        assert_eq!(status, StatusCode::OK, "{reply}");
        let assistant = reply["choices"][0]["message"].clone();
        assert_eq!(
            assistant["tool_calls"][0]["function"]["name"],
            "custom_lookup"
        );
        let messages = request["messages"].as_array_mut().unwrap();
        messages.push(assistant.clone());
        for call in assistant["tool_calls"].as_array().unwrap() {
            messages.push(json!({"role":"tool","tool_call_id":call["id"],"content":"result"}));
        }
        let reply = client
            .post(format!("{gateway_url}/v1/chat/completions"))
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(reply.status(), StatusCode::OK);
        assert_eq!(
            reply.json::<Value>().await.unwrap()["choices"][0]["message"]["content"],
            "done"
        );
        let mut requests = Vec::new();
        for _ in 0..if google { 2 } else { 4 } {
            requests.push(
                tokio::time::timeout(std::time::Duration::from_secs(2), captures.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        let last: Value = serde_json::from_slice(&requests.last().unwrap().2).unwrap();
        if google {
            let contents = &last["request"]["contents"];
            assert_eq!(
                contents[1]["parts"][0]["functionCall"]["id"], "http-g-a",
                "{last}"
            );
            assert_eq!(contents[1]["parts"][1]["functionCall"]["id"], "http-g-b");
            assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "HTTP-SIG");
            assert!(contents[1]["parts"][1].get("thoughtSignature").is_none());
            assert_eq!(
                contents[2]["parts"][0]["functionResponse"]["id"],
                "http-g-a"
            );
            assert_eq!(
                contents[2]["parts"][1]["functionResponse"]["id"],
                "http-g-b"
            );
            let refusal = client
                .post(format!("{gateway_url}/v1/responses"))
                .json(&json!({"model":model,"input":"go","stream":true}))
                .send()
                .await
                .unwrap();
            assert_eq!(refusal.status(), StatusCode::BAD_REQUEST);
            assert_eq!(
                refusal.json::<Value>().await.unwrap()["error"]["param"],
                "stream"
            );
        } else {
            assert_eq!(requests[1].0, "/token");
            assert!(String::from_utf8_lossy(&requests[1].2).contains("refresh_token"));
            assert_eq!(requests[2].1["authorization"], "Bearer fresh-token");
            assert_eq!(last["input"][1]["call_id"], "http-c-a");
            assert_eq!(last["input"][1]["arguments"], "{\"n\":1}");
            assert_eq!(last["input"][2]["call_id"], "http-c-a");
            assert_eq!(last["input"][2]["output"], "result");
        }
        assert!(
            captures.try_recv().is_err(),
            "no unexpected upstream traffic"
        );
        if google {
            let native_request = json!({"contents":[{"role":"model","parts":[
                {"functionCall":{"id":"native-http-a","name":"custom_lookup","args":{}},"thoughtSignature":"NATIVE-HTTP-SIG"},
                {"functionCall":{"id":"native-http-b","name":"edit","args":{}}}
            ]},{"role":"user","parts":[{"functionResponse":{"id":"native-http-a","name":"custom_lookup","response":{"result":"one"}}},{"functionResponse":{"id":"native-http-b","name":"edit","response":{"result":"two"}}}]}]});
            for action in ["generateContent", "streamGenerateContent"] {
                let response = client
                    .post(format!("{gateway_url}/v1beta/models/{model}:{action}"))
                    .json(&native_request)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                assert!(response.text().await.unwrap().contains("done"));
                let (_, _, raw) =
                    tokio::time::timeout(std::time::Duration::from_secs(2), captures.recv())
                        .await
                        .unwrap()
                        .unwrap();
                let captured: Value = serde_json::from_slice(&raw).unwrap();
                assert_eq!(captured["request"]["contents"], native_request["contents"]);
            }
        }
        let compact_request = json!({"model":model,"input":[{"type":"function_call","call_id":"compact-call","name":"custom_lookup","arguments":"{}"},{"type":"function_call_output","call_id":"compact-call","output":"result"}]});
        let response = client
            .post(format!("{gateway_url}/v1/responses/compact"))
            .json(&compact_request)
            .send()
            .await
            .unwrap();
        if google {
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
            assert!(captures.try_recv().is_err());
        } else {
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.json::<Value>().await.unwrap()["output"][0]["encrypted_content"],
                "opaque-fixture"
            );
            let (path, _, raw) = captures.recv().await.unwrap();
            assert_eq!(path, "/backend-api/codex/responses/compact");
            let captured: Value = serde_json::from_slice(&raw).unwrap();
            assert_eq!(captured["input"], compact_request["input"]);
        }
        for task in &cleanup.tasks {
            task.abort();
        }
        for task in cleanup.tasks.drain(..) {
            assert!(task.await.unwrap_err().is_cancelled());
        }
    }
}

#[test]
fn codex_reasoning_survives_every_byte_split() {
    let wire =
        b"data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"think\"}\n\n";
    for split in 0..=wire.len() {
        let mut parser = SseParser::default();
        let mut events = Vec::new();
        parser.push(&wire[..split], &mut events);
        parser.push(&wire[split..], &mut events);
        parser.finish(&mut events);
        assert_eq!(
            events,
            vec![CodexEvent::ReasoningDelta("think".into())],
            "split {split}"
        );
    }
}

#[test]
fn codex_cancel_is_not_silent_success() {
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    parser.push(
        b"data: {\"type\":\"response.cancelled\",\"response\":{\"status\":\"cancelled\"}}\n\n",
        &mut events,
    );
    assert!(matches!(events.as_slice(), [CodexEvent::Failed { .. }]));
}

#[test]
fn codex_incomplete_preserves_machine_reason() {
    let mut parser = SseParser::default();
    let mut events = Vec::new();
    parser.push(b"data: {\"type\":\"response.incomplete\",\"response\":{\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n", &mut events);
    assert_eq!(
        events,
        vec![
            CodexEvent::OutputLimitReached,
            CodexEvent::Completed { usage: None }
        ]
    );
}

#[test]
fn gemini_wrapped_error_is_terminal_even_with_later_stop() {
    let mut decoder = GeminiDecoder::new();
    let mut events = Vec::new();
    decoder.decode(
        br#"{"response":{"error":{"message":"denied"}}}"#,
        &mut events,
    );
    decoder.decode(br#"{"candidates":[{"finishReason":"STOP"}]}"#, &mut events);
    decoder.finish(&mut events);
    assert_eq!(
        events,
        vec![CodexEvent::Failed {
            message: "denied".into()
        }]
    );
}

#[test]
fn gemini_thinking_text_signature_replays_into_tool_history() {
    let reply = gemini_json_to_openai(
        &json!({"candidates":[{"content":{"parts":[
        {"text":"think", "thought":true, "thoughtSignature":"SIG-THINK-REVIEW"},
        {"functionCall":{"id":"review-think", "name":"lookup", "args":{}}}
    ]}, "finishReason":"STOP"}]}),
        "fixture",
        0,
    );
    assert_eq!(reply["choices"][0]["message"]["reasoning_content"], "think");
    let replay = openai_to_gemini(&json!({"model":"fixture", "messages":[
        reply["choices"][0]["message"].clone(),
        {"role":"tool","tool_call_id":"review-think","content":"ok"}
    ]}))
    .unwrap();
    assert_eq!(
        replay["contents"][0]["parts"][0]["thoughtSignature"],
        "SIG-THINK-REVIEW"
    );
}

#[test]
fn gemini_parallel_calls_keep_order_all_received_signatures_and_results() {
    for (suffix, second_signature) in [("first", None), ("all", Some("SIG-SECOND"))] {
        let a = format!("review-a-{suffix}");
        let b = format!("review-b-{suffix}");
        let mut second = json!({"functionCall":{"id":b,"name":"edit","args":{"n":2}}});
        if let Some(sig) = second_signature {
            second["thoughtSignature"] = json!(sig);
        }
        let reply = gemini_json_to_openai(
            &json!({"candidates":[{"content":{"parts":[
            {"functionCall":{"id":a,"name":"read","args":{"n":1}},"thoughtSignature":"SIG-FIRST"}, second
        ]},"finishReason":"STOP"}]}),
            "fixture",
            0,
        );
        let replay = openai_to_gemini(
            &json!({"model":"fixture","messages":[reply["choices"][0]["message"].clone(),
                {"role":"tool","tool_call_id":a,"content":"one"},
                {"role":"tool","tool_call_id":b,"content":"two"}
            ]}),
        )
        .unwrap();
        let parts = &replay["contents"][0]["parts"];
        assert_eq!(parts[0]["functionCall"]["id"], a);
        assert_eq!(parts[1]["functionCall"]["id"], b);
        assert_eq!(parts[0]["thoughtSignature"], "SIG-FIRST");
        assert_eq!(
            parts[1].get("thoughtSignature").and_then(|v| v.as_str()),
            second_signature
        );
        let results = &replay["contents"][1]["parts"];
        assert_eq!(results[0]["functionResponse"]["id"], a);
        assert_eq!(results[1]["functionResponse"]["id"], b);
        assert_eq!(
            results[0]["functionResponse"]["response"],
            json!({"result":"one"})
        );
        assert_eq!(
            results[1]["functionResponse"]["response"],
            json!({"result":"two"})
        );
    }
}

#[test]
fn codex_chat_tool_roundtrip_preserves_ids_arguments_and_results() {
    let request = json!({"model":"fixture","messages":[
        {"role":"assistant","tool_calls":[{"id":"review-codex","type":"function","function":{"name":"lookup","arguments":"{\"city\":\"서울\"}"}}]},
        {"role":"tool","tool_call_id":"review-codex","content":"21C"}
    ]});
    let translated = compat::openai_to_codex(&serde_json::to_vec(&request).unwrap()).unwrap();
    let body: serde_json::Value = serde_json::from_slice(&translated.body).unwrap();
    assert_eq!(
        body["input"],
        json!([
            {"type":"function_call","call_id":"review-codex","name":"lookup","arguments":"{\"city\":\"서울\"}"},
            {"type":"function_call_output","call_id":"review-codex","output":"21C"}
        ])
    );
}

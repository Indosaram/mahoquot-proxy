#[cfg(test)]
mod tests {
    use mahoquot_gateway::plugins::abi::*;
    use mahoquot_gateway::plugins::loader::{PluginClient, PluginHost};
    use serde_json::json;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    struct TestMockClient {
        registered: AtomicBool,
        call_count: AtomicUsize,
    }

    impl PluginClient for TestMockClient {
        fn call(&self, method: &str, request: &[u8]) -> Result<Vec<u8>, PluginError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            match method {
                METHOD_PLUGIN_REGISTER => {
                    self.registered.store(true, Ordering::SeqCst);
                    let res = json!({
                        "schema_version": 6,
                        "metadata": {
                            "Name": "test-plugin",
                            "Version": "1.0.0",
                            "Author": "test-author",
                            "GitHubRepository": "https://github.com/test/test",
                            "Logo": "https://example.com/logo.png",
                            "ConfigFields": []
                        },
                        "capabilities": {
                            "request_interceptor": true,
                            "request_normalizer": true,
                            "request_translator": true,
                            "response_interceptor": true,
                            "response_before_translator": true,
                            "response_translator": true,
                            "response_after_translator": true,
                            "stream_chunk_interceptor": true,
                            "request_lifecycle_plugin": true,
                            "websocket_response_observer": true,
                            "quota_provider": true
                        }
                    });
                    let env = Envelope::success(res);
                    Ok(serde_json::to_vec(&env).unwrap())
                }
                METHOD_PLUGIN_QUIESCE => {
                    let env = Envelope::success(json!({"status": "quiesced"}));
                    Ok(serde_json::to_vec(&env).unwrap())
                }
                METHOD_REQUEST_INTERCEPT_BEFORE | METHOD_REQUEST_INTERCEPT_AFTER => {
                    let resp = json!({
                        "path": "/v1/test/override",
                        "headers": {
                            "x-plugin-injected": ["true"]
                        },
                        "body": request
                    });
                    let env = Envelope::success(resp);
                    Ok(serde_json::to_vec(&env).unwrap())
                }
                METHOD_RESPONSE_INTERCEPT_AFTER => {
                    let resp = json!({
                        "headers": {
                            "x-response-injected": ["active"]
                        },
                        "body": request
                    });
                    let env = Envelope::success(resp);
                    Ok(serde_json::to_vec(&env).unwrap())
                }
                METHOD_REQUEST_NORMALIZE
                | METHOD_REQUEST_TRANSLATE
                | METHOD_RESPONSE_NORMALIZE_BEFORE
                | METHOD_RESPONSE_TRANSLATE
                | METHOD_RESPONSE_NORMALIZE_AFTER => {
                    use base64::Engine;
                    let encoded =
                        base64::engine::general_purpose::STANDARD.encode(b"transformed-payload");
                    let resp = json!({
                        "Body": encoded
                    });
                    let env = Envelope::success(resp);
                    Ok(serde_json::to_vec(&env).unwrap())
                }
                METHOD_RESPONSE_INTERCEPT_STREAM_CHUNK => {
                    let resp = json!({
                        "headers": {
                            "x-chunk-injected": ["yes"]
                        },
                        "body": [1, 2, 3],
                        "drop_chunk": false
                    });
                    let env = Envelope::success(resp);
                    Ok(serde_json::to_vec(&env).unwrap())
                }
                METHOD_REQUEST_COMPLETE => {
                    let env = Envelope::success(json!({"acknowledged": true}));
                    Ok(serde_json::to_vec(&env).unwrap())
                }
                METHOD_WEBSOCKET_RESPONSE_EVENT => {
                    let env = Envelope::success(json!({"observed": true}));
                    Ok(serde_json::to_vec(&env).unwrap())
                }
                _ => {
                    let env = Envelope::error("unknown_method", "unknown method", Some(404));
                    Ok(serde_json::to_vec(&env).unwrap())
                }
            }
        }

        fn shutdown(&self) {
            self.registered.store(false, Ordering::SeqCst);
        }
    }

    #[test]
    fn plugin_registration_and_quiesce_lifecycle() {
        let callback_handler = Arc::new(
            |_plugin_id: &str, method: &str, _req: &[u8]| -> Result<Vec<u8>, PluginError> {
                if method == METHOD_HOST_LOG {
                    return Ok(b"{}".to_vec());
                }
                Err(PluginError {
                    code: "unsupported".to_string(),
                    message: "unsupported".to_string(),
                    retryable: false,
                    http_status: Some(500),
                })
            },
        );

        let host = PluginHost::new(callback_handler);
        let mock_client = Arc::new(TestMockClient {
            registered: AtomicBool::new(false),
            call_count: AtomicUsize::new(0),
        });

        let plugin = host
            .register_plugin_client(
                "test-plugin",
                Path::new("/tmp/test-plugin.so"),
                "1.0.0",
                mock_client.clone(),
                Arc::new(AtomicUsize::new(0)),
            )
            .expect("register plugin");

        assert_eq!(plugin.id, "test-plugin");
        assert_eq!(plugin.schema_version, 6);
        assert_eq!(plugin.name, "test-plugin");
        assert!(mock_client.registered.load(Ordering::SeqCst));

        assert!(!host.is_plugin_busy("test-plugin"));

        let uninstalled = host.unload_plugin("test-plugin");
        assert!(uninstalled);
        assert!(!mock_client.registered.load(Ordering::SeqCst));
        assert!(host.get_plugin("test-plugin").is_none());
    }

    #[test]
    fn plugin_hooks_intercept_and_normalize() {
        let callback_handler = Arc::new(|_: &str, _: &str, _: &[u8]| Ok(vec![]));
        let host = Arc::new(PluginHost::new(callback_handler));
        let mock_client = Arc::new(TestMockClient {
            registered: AtomicBool::new(false),
            call_count: AtomicUsize::new(0),
        });

        let _plugin = host
            .register_plugin_client(
                "test-plugin",
                Path::new("/tmp/test-plugin.so"),
                "1.0.0",
                mock_client.clone(),
                Arc::new(AtomicUsize::new(0)),
            )
            .expect("register plugin");

        let hooks = mahoquot_gateway::plugins::hooks::PluginHookRegistry::new(host);

        let intercept_req = mahoquot_gateway::plugins::hooks::RequestInterceptRequest {
            request_id: "req-123".to_string(),
            trace_id: "trace-456".to_string(),
            source_format: "openai".to_string(),
            to_format: "anthropic".to_string(),
            model: "claude-3-5".to_string(),
            requested_model: "claude-3-5".to_string(),
            stream: false,
            headers: HashMap::new(),
            body: b"original-body".to_vec(),
            metadata: HashMap::new(),
        };

        let before_res = hooks.intercept_request_before_auth(intercept_req.clone());
        assert_eq!(before_res.path, "/v1/test/override");
        assert_eq!(
            before_res.headers.get("x-plugin-injected").unwrap(),
            &vec!["true".to_string()]
        );

        let after_res = hooks.intercept_request_after_auth(intercept_req.clone());
        assert_eq!(after_res.path, "/v1/test/override");

        let norm_req = hooks.normalize_request("openai", "anthropic", "claude-3-5", b"test", false);
        assert_eq!(norm_req, b"transformed-payload");

        let (trans_req, handled) =
            hooks.translate_request("openai", "anthropic", "claude-3-5", b"test", false);
        assert!(handled);
        assert_eq!(trans_req, b"transformed-payload");

        let response_intercept_req = mahoquot_gateway::plugins::hooks::ResponseInterceptRequest {
            request_id: "req-123".to_string(),
            source_format: "openai".to_string(),
            model: "claude-3-5".to_string(),
            requested_model: "claude-3-5".to_string(),
            stream: false,
            request_headers: HashMap::new(),
            response_headers: HashMap::new(),
            original_request: vec![],
            request_body: vec![],
            body: b"upstream-response".to_vec(),
            status_code: 200,
            metadata: HashMap::new(),
        };

        let resp_intercept_res = hooks.intercept_response(response_intercept_req);
        assert_eq!(
            resp_intercept_res
                .headers
                .get("x-response-injected")
                .unwrap(),
            &vec!["active".to_string()]
        );

        let chunk_req = mahoquot_gateway::plugins::hooks::StreamChunkInterceptRequest {
            request_id: "req-123".to_string(),
            source_format: "openai".to_string(),
            model: "claude-3-5".to_string(),
            requested_model: "claude-3-5".to_string(),
            request_headers: HashMap::new(),
            response_headers: HashMap::new(),
            original_request: vec![],
            request_body: vec![],
            body: vec![9, 9, 9],
            history_chunks: vec![],
            chunk_index: 0,
            metadata: HashMap::new(),
        };

        let chunk_res = hooks.intercept_stream_chunk(chunk_req);
        assert_eq!(
            chunk_res.headers.get("x-chunk-injected").unwrap(),
            &vec!["yes".to_string()]
        );
        assert_eq!(chunk_res.body, vec![1, 2, 3]);
    }
}

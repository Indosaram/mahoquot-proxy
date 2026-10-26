use super::*;

fn execution(
    phase: &str,
    index: usize,
    account: &str,
    socket: &str,
    status: u16,
) -> WorkloadExecution {
    WorkloadExecution {
        phase: phase.into(),
        request_id: format!("{phase}:{index}"),
        account: account.into(),
        connection_id: Some(socket.into()),
        status,
    }
}

#[test]
fn b4_requires_correlated_fault_recovery_and_unchanged_b() {
    let mut rows = vec![
        execution("B4", 0, "a", "a1", 200),
        execution("B4", 1, "b", "b1", 200),
        execution("B4", 2, "a", "a1", 503),
        execution("B4", 2, "c", "c1", 200),
        execution("B4", 3, "b", "b1", 200),
        execution("B4", 4, "c", "c1", 200),
        execution("B4", 5, "b", "b1", 200),
    ];
    assert!(verify_b4(&rows).is_ok());
    for row in &mut rows {
        row.connection_id = None;
    }
    assert!(verify_b4(&rows).is_ok());
    rows[2].status = 200;
    assert!(verify_b4(&rows).is_err());
    rows[2].status = 503;
    rows[4].account = "changed".into();
    assert!(verify_b4(&rows).is_err());
}

#[test]
fn b9_requires_two_real_accounts_with_isolated_reused_sockets() {
    let mut rows = vec![
        execution("B9", 0, "a", "s1", 200),
        execution("B9", 1, "a", "s1", 200),
        execution("B9", 2, "b", "s2", 200),
        execution("B9", 3, "b", "s2", 200),
    ];
    assert!(verify_b9(&rows).is_ok());
    rows[3].connection_id = Some("s3".into());
    assert!(verify_b9(&rows).is_err());
    rows[3].connection_id = Some("s2".into());
    rows[2].account = "a".into();
    rows[3].account = "a".into();
    assert!(verify_b9(&rows).is_err());
    assert!(verify_b9(&[]).is_err());
}

#[test]
fn mixed_phase_stats_accept_null_socket_but_b9_requires_observed_socket() {
    let mut fixture = serde_json::to_value(MockObservedStats::default()).unwrap();
    fixture["workload_executions"] = serde_json::json!([
        {"phase":"B5", "request_id":"B5:0", "account":"tenant1", "connection_id":null, "status":200},
        {"phase":"B9", "request_id":"B9:0", "account":"a", "connection_id":"s1", "status":200},
        {"phase":"B9", "request_id":"B9:1", "account":"a", "connection_id":"s1", "status":200},
        {"phase":"B9", "request_id":"B9:2", "account":"b", "connection_id":"s2", "status":200},
        {"phase":"B9", "request_id":"B9:3", "account":"b", "connection_id":"s2", "status":200}
    ]);
    let parsed: MockObservedStats = serde_json::from_value(fixture.clone()).unwrap();
    assert!(parsed.workload_executions[0].connection_id.is_none());
    assert!(verify_b9(&parsed.workload_executions[1..]).is_ok());
    for unavailable in [serde_json::Value::Null, serde_json::json!("")] {
        fixture["workload_executions"][1]["connection_id"] = unavailable;
        let parsed: MockObservedStats = serde_json::from_value(fixture.clone()).unwrap();
        assert!(verify_b9(&parsed.workload_executions[1..]).is_err());
    }
}

#[test]
fn b5_requires_normal_second_tenant_after_parent_and_child() {
    let mut rows: Vec<_> = (0..3)
        .map(|index| MockObservedTurn {
            phase: "b5".into(),
            turn: index + 1,
            account: if index == 2 { "b" } else { "a" }.into(),
            routing_headers_leaked: false,
            signature_present: false,
            tenant: if index == 2 { "tenant2" } else { "tenant1" }.into(),
            requested_tenant: if index == 2 { "tenant2" } else { "tenant1" }.into(),
        })
        .collect();
    assert!(verify_b5(&rows, 1).is_ok());
    assert!(verify_b5(&rows[..2], 1).is_err());
    rows[2].account = "a".into();
    assert!(verify_b5(&rows, 1).is_err());
}

#[test]
fn locality_raw_preserves_bytes_and_semantic_timing_after_error() {
    let mut sample = LocalityRequestObservation {
        request_id: "B4:0".into(),
        phase: "B4".into(),
        headers_ms: Some(1.0),
        content_ms: None,
        end_ms: None,
        status: Some(200),
        error: None,
        semantic_error: None,
        response_body: Vec::new(),
    };
    let mut gate = SemanticGate::default();
    sample.feed(&mut gate, b": keepalive\n\n", 2.0);
    sample.feed(
        &mut gate,
        b"data: {\"choices\":[{\"delta\":{\"content\":\"x",
        3.0,
    );
    assert_eq!(sample.content_ms, None);
    sample.feed(&mut gate, b"x\"}}]}\n\n", 4.0);
    sample.feed(&mut gate, b"data: {\"error\":\"failed\"}\n\n", 5.0);
    sample.feed(&mut gate, b"tail", 6.0);
    assert_eq!(sample.content_ms, Some(4.0));
    assert!(sample.semantic_error.is_some());
    assert!(sample.response_body.ends_with(b"tail"));
    let value = serde_json::to_value(sample).unwrap();
    assert_eq!(value["phase"], "B4");
    assert_eq!(value["request_id"], "B4:0");
    assert!(value["end_ms"].is_null());
    assert_eq!(value["headers_ms"], 1.0);
}

#[test]
fn failed_phase_retains_attempt_and_no_fabricated_resources() {
    let mut phase = LocalityPhaseObservations::new("B5");
    phase.requests.push(LocalityRequestObservation {
        request_id: "B5:0".into(),
        phase: "B5".into(),
        headers_ms: None,
        content_ms: None,
        end_ms: Some(1.0),
        status: None,
        error: Some("connection failed".into()),
        semantic_error: None,
        response_body: Vec::new(),
    });
    let _: B5HierarchyResult = phase.complete(Err("request failed".into()));
    let value = serde_json::to_value(phase).unwrap();
    assert_eq!(value["requests"].as_array().unwrap().len(), 1);
    assert!(value["requests"][0]["headers_ms"].is_null());
    assert!(value["requests"][0]["content_ms"].is_null());
    assert!(value["failure"].is_string());
    assert!(value["resources"].as_array().unwrap().is_empty());
}

#[test]
fn acknowledgement_requires_matching_phase_and_owned_pid() {
    assert!(validate_phase_ack(
        "MAHOQUOT_BENCH_PHASE_ACK {\"phase\":\"B3\",\"pid\":42}\n",
        "B3",
        42
    )
    .is_ok());
    for invalid in [
        "",
        "MAHOQUOT_BENCH_PHASE_ACK {}",
        "MAHOQUOT_BENCH_PHASE_ACK {\"phase\":\"B4\",\"pid\":42}",
        "MAHOQUOT_BENCH_PHASE_ACK {\"phase\":\"B3\",\"pid\":43}",
        "MAHOQUOT_BENCH_PHASE_ACK {\"phase\":\"B3\",\"pid\":\"42\"}",
    ] {
        assert!(validate_phase_ack(invalid, "B3", 42).is_err());
    }
}

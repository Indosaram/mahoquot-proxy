use mahoquot_gateway::home::{
    is_home_authoritative, set_active_home_client, ConcurrencyReleaseFrame, HomeClient, HomeConfig,
    HomeTLSConfig,
};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

#[test]
fn test_home_integration_loopback_workflow() {
    let listener = (18840..=18899)
        .find_map(|port| TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).ok())
        .expect("reserved test port");
    let port = listener.local_addr().unwrap().port();

    let server_thread = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() || line.is_empty() {
                return;
            }
            stream.write_all(b"+OK\r\n").expect("RESP response");
            stream.flush().expect("response flush");
        }
    });

    let config = HomeConfig {
        enabled: true,
        node_id: "test-node-32".to_string(),
        host: "127.0.0.1".to_string(),
        port,
        disable_cluster_discovery: true,
        tls: HomeTLSConfig::default(),
    };

    let client = HomeClient::new(config);
    let models = br#"{"data":[{"id":"home-only","owned_by":"codex"}]}"#.to_vec();
    assert!(client
        .apply_authoritative_snapshot(br#"{"revision":2,"config":{}}"#.to_vec(), models.clone())
        .unwrap());
    assert!(!client
        .apply_authoritative_snapshot(br#"{"revision":1,"config":{}}"#.to_vec(), models)
        .unwrap());
    assert_eq!(
        client
            .applied_revision
            .load(std::sync::atomic::Ordering::Acquire),
        2
    );
    set_active_home_client(Some(client.clone()));
    assert!(!is_home_authoritative());

    client
        .heartbeat_ok
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(is_home_authoritative());

    let frame = ConcurrencyReleaseFrame {
        credential_id: "cred-1".to_string(),
        model: "gpt-4".to_string(),
        release_seq: 1,
    };
    let release_res = client.push_concurrency_release(&frame);
    assert!(release_res.is_ok());

    assert!(!client.dispatch_fenced());
    client.abort_ambiguous_dispatch();
    assert!(client.dispatch_fenced());
    assert!(!is_home_authoritative());

    let post_fenced_res = client.push_concurrency_release(&frame);
    assert!(post_fenced_res.is_err());

    set_active_home_client(None);
    server_thread.join().expect("Home server joined");
}

use bytes::Bytes;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use mahoquot_gateway::management::gate::ManagementAttempts;
use mahoquot_gateway::management::settings::Settings;
use mahoquot_gateway::management::store::SettingsStore;
use mahoquot_gateway::resp::global_redis_hub;
use mahoquot_gateway::server::{run_protocol_server, ProtocolListener};

#[tokio::test]
async fn test_same_port_http_and_resp() {
    let mut settings = Settings::default();
    settings.remote_management.secret_key = "test-secret".to_string();
    let store = Arc::new(SettingsStore::new(settings, std::path::PathBuf::new()));
    let attempts = Arc::new(std::sync::Mutex::new(ManagementAttempts::default()));

    let tcp_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp_listener.local_addr().unwrap();

    let listener = ProtocolListener::new(tcp_listener, &store)
        .await
        .unwrap()
        .with_management_context(Arc::clone(&store), Arc::clone(&attempts));

    let app = axum::Router::new().route("/healthz", axum::routing::get(|| async { "ok" }));

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_handle = tokio::spawn(async move {
        run_protocol_server(listener, app, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    let client = reqwest::Client::new();
    let res = client
        .get(format!("http://{addr}/healthz"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    stream.write_all(b"*1\r\n$4\r\nLLEN\r\n").await.unwrap();
    stream.flush().await.unwrap();
    let mut buf = vec![0u8; 128];
    let n = stream.read(&mut buf).await.unwrap();
    let reply = String::from_utf8_lossy(&buf[..n]);
    assert!(reply.starts_with("-NOAUTH"));

    stream
        .write_all(b"*2\r\n$4\r\nAUTH\r\n$11\r\ntest-secret\r\n")
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let n = stream.read(&mut buf).await.unwrap();
    let reply = String::from_utf8_lossy(&buf[..n]);
    assert_eq!(reply, "+OK\r\n");

    stream.write_all(b"*1\r\n$4\r\nPING\r\n").await.unwrap();
    stream.flush().await.unwrap();
    let n = stream.read(&mut buf).await.unwrap();
    let reply = String::from_utf8_lossy(&buf[..n]);
    assert_eq!(reply, "+PONG\r\n");

    let hub = global_redis_hub();
    hub.enqueue_usage(Bytes::from_static(b"{\"event_id\":\"test-1\"}"));

    stream
        .write_all(b"*2\r\n$4\r\nLLEN\r\n$5\r\nusage\r\n")
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let n = stream.read(&mut buf).await.unwrap();
    let reply = String::from_utf8_lossy(&buf[..n]);
    assert!(reply.starts_with(':'));

    // The queue is the process-wide hub, and the HTTP request above records its own
    // usage event, so pop until our event appears instead of assuming it is first.
    let queued: usize = reply
        .trim()
        .trim_start_matches(':')
        .parse()
        .expect("LLEN integer reply");
    assert!(queued >= 1, "usage queue must hold the enqueued event");
    let mut big = vec![0u8; 64 * 1024];
    let mut found = false;
    let mut replies = Vec::new();
    for _ in 0..queued {
        stream
            .write_all(b"*2\r\n$4\r\nLPOP\r\n$5\r\nusage\r\n")
            .await
            .unwrap();
        stream.flush().await.unwrap();
        // A bulk reply ("$<len>\r\n<payload>\r\n") may arrive in several TCP reads;
        // read until the declared payload is complete instead of trusting one read.
        let mut n = stream.read(&mut big).await.unwrap();
        loop {
            let head = String::from_utf8_lossy(&big[..n]).into_owned();
            let complete = match head
                .strip_prefix('$')
                .and_then(|rest| rest.split_once("\r\n"))
            {
                Some(("-1", _)) => true,
                Some((len, body)) => len
                    .parse::<usize>()
                    .map_or(true, |len| body.len() >= len + 2),
                None => head.ends_with("\r\n"),
            };
            if complete {
                break;
            }
            let more = stream.read(&mut big[n..]).await.unwrap();
            assert!(more > 0, "connection closed mid-reply: {head:?}");
            n += more;
        }
        let popped = String::from_utf8_lossy(&big[..n]).into_owned();
        if popped.contains("test-1") {
            found = true;
            break;
        }
        replies.push(popped);
    }
    assert!(
        found,
        "LPOP must return the enqueued usage event; LLEN={queued} replies={replies:?}"
    );

    let _ = shutdown_tx.send(());
    let _ = server_handle.await;
}

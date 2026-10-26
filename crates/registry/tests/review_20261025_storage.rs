use hmac::{Hmac, Mac};
use mahoquot_registry::store::{ObjectStore, PostgresStore, StorageBackend, StorageManager};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{mpsc, Arc};
use std::time::Duration;

struct Root(std::path::PathBuf);
impl Root {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "review-storage-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir(&root).unwrap();
        Self(root)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

// Own the worker even if a client assertion unwinds before receiving its result.
struct FixtureDone {
    receiver: mpsc::Receiver<Vec<String>>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl FixtureDone {
    fn recv_timeout(&self, timeout: Duration) -> Result<Vec<String>, mpsc::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }
}

impl Drop for FixtureDone {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            // Completion may already have closed the cancellation receiver.
            let _ = cancel.send(());
        }
        if let Some(worker) = self.worker.take() {
            let result = worker.join();
            if !std::thread::panicking() {
                result.expect("fixture worker panicked");
            }
        }
    }
}

// S3 uses RFC 3986 encoding, not application/x-www-form-urlencoded encoding.
// This oracle deliberately does not call any production signing/URL helpers.
fn sigv4_encode(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            encoded.push('%');
            encoded.push(char::from(HEX[usize::from(byte >> 4)]));
            encoded.push(char::from(HEX[usize::from(byte & 15)]));
        }
    }
    encoded
}

fn verify_sigv4(header: &str, body: &[u8], host: &str, region: &str) -> Result<(), String> {
    let fail = || "invalid SigV4 wire request".to_owned();
    let mut lines = header.split("\r\n");
    let request: Vec<_> = lines.next().ok_or_else(fail)?.split_whitespace().collect();
    if request.len() != 3 || request[2] != "HTTP/1.1" {
        return Err(fail());
    }
    let mut headers = std::collections::BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or_else(fail)?;
        if headers
            .insert(name.to_ascii_lowercase(), value.trim())
            .is_some()
        {
            return Err(fail());
        }
    }
    let field = |name: &str| headers.get(name).copied().ok_or_else(fail);
    let digest = hex::encode(Sha256::digest(body));
    if field("host")? != host || field("x-amz-content-sha256")? != digest {
        return Err(fail());
    }
    let date = field("x-amz-date")?;
    // Parse the captured timestamp, not a separately sampled wall clock.
    chrono::NaiveDateTime::parse_from_str(date, "%Y%m%dT%H%M%SZ").map_err(|_| fail())?;
    if date.len() != 16 || !date.is_ascii() {
        return Err(fail());
    }
    let scope = format!("{}/{region}/s3/aws4_request", &date[..8]);
    let authorization = field("authorization")?
        .strip_prefix("AWS4-HMAC-SHA256 ")
        .ok_or_else(fail)?;
    let parts: Vec<_> = authorization.split(", ").collect();
    let signed = "host;x-amz-content-sha256;x-amz-date";
    if parts.len() != 3
        || parts[0] != format!("Credential=fixture/{scope}")
        || parts[1] != format!("SignedHeaders={signed}")
    {
        return Err(fail());
    }
    let signature =
        hex::decode(parts[2].strip_prefix("Signature=").ok_or_else(fail)?).map_err(|_| fail())?;
    let (path, query) = request[1].split_once('?').unwrap_or((request[1], ""));
    if !path.starts_with('/') || path.contains('#') {
        return Err(fail());
    }
    // Decode the transmitted form query, then encode and sort BOTH names and
    // values as required by SigV4 (wire parameter order is not canonical).
    let mut pairs: Vec<_> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(name, value)| (sigv4_encode(&name), sigv4_encode(&value)))
        .collect();
    pairs.sort();
    let canonical_query = pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let canonical = format!(
        "{}\n{path}\n{canonical_query}\nhost:{host}\nx-amz-content-sha256:{digest}\nx-amz-date:{date}\n\n{signed}\n{digest}",
        request[0]
    );
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical.as_bytes()))
    );
    let mut key = b"AWS4fixture".to_vec();
    for component in [&date[..8], region, "s3", "aws4_request"] {
        let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|_| fail())?;
        mac.update(component.as_bytes());
        key = mac.finalize().into_bytes().to_vec();
    }
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).map_err(|_| fail())?;
    mac.update(to_sign.as_bytes());
    mac.verify_slice(&signature).map_err(|_| fail())
}

fn fixture(responses: Vec<(u16, String)>) -> (String, FixtureDone) {
    let listener = (18840..=18899)
        .find_map(|port| TcpListener::bind(("127.0.0.1", port)).ok())
        .expect("available fixture port");
    let address = format!("http://{}", listener.local_addr().unwrap());
    let host = listener.local_addr().unwrap().to_string();
    let region = std::env::var("OBJECTSTORE_REGION").unwrap_or_else(|_| "us-east-1".into());
    let (done_tx, done_rx) = mpsc::channel();
    let (cancel_tx, mut cancel_rx) = tokio::sync::oneshot::channel();
    let worker = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = {
            let _guard = runtime.enter();
            tokio::net::TcpListener::from_std(listener).unwrap()
        };
        let mut requests = Vec::new();
        for (status, body) in responses {
            let accepted = runtime.block_on(async {
                tokio::select! {
                    _ = &mut cancel_rx => None,
                    accepted = tokio::time::timeout(Duration::from_secs(5), listener.accept()) => {
                        Some(accepted.expect("fixture accept deadline").unwrap())
                    }
                }
            });
            let Some((socket, _)) = accepted else { return };
            let mut socket = socket.into_std().unwrap();
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
                assert!(request.len() < 32768);
            }
            let header = String::from_utf8(request).unwrap();
            let length = header
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut payload = vec![0; length];
            socket.read_exact(&mut payload).unwrap();
            verify_sigv4(&header, &payload, &host, &region)
                .expect("independent server-side SigV4 verification");
            // Keep the negative control structurally valid: change exactly one
            // hex digit, rather than merely rejecting a malformed auth header.
            let signature = header.find("Signature=").unwrap() + "Signature=".len();
            let mut forged = header.clone();
            let replacement = if &header[signature..signature + 1] == "0" {
                "1"
            } else {
                "0"
            };
            forged.replace_range(signature..signature + 1, replacement);
            assert!(verify_sigv4(&forged, &payload, &host, &region).is_err());
            let mut altered_body = payload.clone();
            altered_body.push(0);
            assert!(verify_sigv4(&header, &altered_body, &host, &region).is_err());
            requests.push(header.lines().next().unwrap().to_owned());
            write!(socket, "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        // A client assertion can unwind before consuming the completion event.
        let _ = done_tx.send(requests);
    });
    (
        address,
        FixtureDone {
            receiver: done_rx,
            cancel: Some(cancel_tx),
            worker: Some(worker),
        },
    )
}

fn object(endpoint: &str) -> ObjectStore {
    ObjectStore::new(endpoint, "bucket", "fixture", "fixture", "").unwrap()
}

#[test]
fn sigv4_wire_authenticates_escaped_crud_and_paginated_query() {
    let (endpoint, done) = fixture(vec![
        (200, String::new()),
        (200, "stored".into()),
        (204, String::new()),
        (200, "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>next +/&amp;=%?#</NextContinuationToken></ListBucketResult>".into()),
        (200, "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>".into()),
    ]);
    let store = ObjectStore::new(&endpoint, "bucket", "fixture", "fixture", "team & one").unwrap();
    let key = "auths/a +#?%.json";
    // Nonempty, non-text bytes distinguish payload hashing from the empty hash.
    store.put(key, b"\0signed\xff\r\nbody").unwrap();
    assert_eq!(store.get(key).unwrap().unwrap(), b"stored");
    store.delete(key).unwrap();
    assert!(store.list("auths/a +").unwrap().is_empty());
    let requests = done.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(requests.len(), 5);
    for (request, method) in requests[..3].iter().zip(["PUT", "GET", "DELETE"]) {
        assert_eq!(
            request,
            &format!("{method} /bucket/team%20%26%20one/auths/a%20%2B%23%3F%25.json HTTP/1.1")
        );
    }
    for (index, request) in requests[3..].iter().enumerate() {
        let target = request.split_whitespace().nth(1).unwrap();
        let url = url::Url::parse(&format!("{endpoint}{target}")).unwrap();
        assert_eq!(url.path(), "/bucket");
        let mut query: Vec<_> = url.query_pairs().into_owned().collect();
        query.sort();
        let mut expected = vec![
            ("list-type".to_owned(), "2".to_owned()),
            ("prefix".to_owned(), "team & one/auths/a +".to_owned()),
        ];
        if index == 1 {
            expected.push(("continuation-token".into(), "next +/&=%?#".into()));
        }
        expected.sort();
        assert_eq!(query, expected);
    }
}

#[test]
fn remote_failure_preserves_spool_and_returns_error() {
    for status in [403, 500] {
        let root = Root::new();
        let spool = root.0.join("spool");
        fs::create_dir_all(spool.join("auths")).unwrap();
        fs::write(spool.join("auths/kept.json"), b"original").unwrap();
        let (endpoint, done) = fixture(vec![(status, String::new()); 4]);
        let backend = Arc::new(object(&endpoint));
        let manager = StorageManager::new(backend.clone(), &spool).unwrap();
        assert!(manager.sync_all_to_spool().is_err());
        assert!(manager.get("auths/kept.json").is_err());
        assert!(manager.delete("auths/kept.json").is_err());
        assert!(manager.put("auths/new.json", b"pending").is_err());
        assert_eq!(
            fs::read(spool.join("auths/kept.json")).unwrap(),
            b"original"
        );
        assert!(!spool.join("auths/new.json").exists());
        assert_eq!(
            fs::read(manager.pending_root().join("auths/new.json")).unwrap(),
            b"pending"
        );
        assert_eq!(done.recv_timeout(Duration::from_secs(10)).unwrap().len(), 4);
    }
}

#[test]
fn postgres_without_runtime_never_acknowledges_memory_durability() {
    let pg = PostgresStore::new("postgres://fixture@127.0.0.1:18899/fixture", None);
    assert!(pg.get("auths/a.json").is_err());
    assert!(pg.put("auths/a.json", b"{}").is_err());
    assert!(pg.delete("auths/a.json").is_err());
    assert!(pg.list("").is_err());
    assert!(pg.save_cooldown("a", "model", b"{}", false).is_err());
    assert!(pg.load_cooldowns().is_err());
}

#[test]
fn incomplete_inventory_preserves_spool() {
    for xml in [
        "<ListBucketResult><IsTruncated>true</IsTruncated></ListBucketResult>",
        "<Other><IsTruncated>false</IsTruncated></Other>",
        "<ListBucketResult><IsTruncated>false</IsTruncated>",
    ] {
        let root = Root::new();
        let spool = root.0.join("spool");
        fs::create_dir_all(&spool).unwrap();
        fs::write(spool.join("kept"), b"original").unwrap();
        let (endpoint, done) = fixture(vec![(200, xml.into())]);
        let manager = StorageManager::new(Arc::new(object(&endpoint)), &spool).unwrap();
        assert!(manager.sync_all_to_spool().is_err());
        assert_eq!(fs::read(spool.join("kept")).unwrap(), b"original");
        done.recv_timeout(Duration::from_secs(10)).unwrap();
    }
}

#[test]
fn paginated_inventory_decodes_entities_and_continuation() {
    let (endpoint, done) = fixture(vec![
        (200, "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>a+b&amp;c</NextContinuationToken><Contents><Key>auths/a&amp;b.json</Key></Contents></ListBucketResult>".into()),
        (200, "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>auths/z.json</Key></Contents></ListBucketResult>".into()),
    ]);
    assert_eq!(
        object(&endpoint).list("auths/").unwrap(),
        ["auths/a&b.json", "auths/z.json"]
    );
    let requests = done.recv_timeout(Duration::from_secs(10)).unwrap();
    let query = requests[1].split_whitespace().nth(1).unwrap();
    let parsed = url::Url::parse(&format!("{endpoint}{query}")).unwrap();
    assert!(parsed
        .query_pairs()
        .any(|(key, value)| key == "continuation-token" && value == "a+b&c"));
}

#[test]
fn complete_paginated_sync_encodes_keys_and_preserves_prefix() {
    let root = Root::new();
    let spool = root.0.join("spool");
    fs::create_dir(&spool).unwrap();
    fs::write(spool.join("obsolete"), b"old").unwrap();
    let (endpoint, done) = fixture(vec![
        (200, "<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><IsTruncated>true</IsTruncated><NextContinuationToken>next+&amp;</NextContinuationToken><Contents><Key>team &amp; one/auths/a#?%.json</Key></Contents></ListBucketResult>".into()),
        (200, "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>team &amp; one/auths/z.json</Key></Contents></ListBucketResult>".into()),
        (200, "first".into()),
        (200, "second".into()),
    ]);
    let backend =
        ObjectStore::new(&endpoint, "bucket", "fixture", "fixture", "team & one").unwrap();
    let manager = StorageManager::new(Arc::new(backend), &spool).unwrap();
    manager.sync_all_to_spool().unwrap();
    assert_eq!(fs::read(spool.join("auths/a#?%.json")).unwrap(), b"first");
    assert_eq!(fs::read(spool.join("auths/z.json")).unwrap(), b"second");
    assert!(!spool.join("obsolete").exists());
    let requests = done.recv_timeout(Duration::from_secs(10)).unwrap();
    for request in &requests[..2] {
        let target = request.split_whitespace().nth(1).unwrap();
        let url = url::Url::parse(&format!("{endpoint}{target}")).unwrap();
        assert!(url
            .query_pairs()
            .any(|(key, value)| key == "prefix" && value == "team & one/"));
    }
    assert_eq!(
        requests[2],
        "GET /bucket/team%20%26%20one/auths/a%23%3F%25.json HTTP/1.1"
    );
}

#[test]
fn failed_or_repeated_continuation_preserves_spool() {
    let first = "<ListBucketResult><IsTruncated>true</IsTruncated><NextContinuationToken>repeat</NextContinuationToken></ListBucketResult>";
    for second in [(500, String::new()), (200, first.into())] {
        let root = Root::new();
        let spool = root.0.join("spool");
        fs::create_dir_all(&spool).unwrap();
        fs::write(spool.join("kept"), b"original").unwrap();
        let (endpoint, done) = fixture(vec![(200, first.into()), second]);
        let manager = StorageManager::new(Arc::new(object(&endpoint)), &spool).unwrap();
        assert!(manager.sync_all_to_spool().is_err());
        assert_eq!(fs::read(spool.join("kept")).unwrap(), b"original");
        assert_eq!(done.recv_timeout(Duration::from_secs(10)).unwrap().len(), 2);
    }
}

#[test]
fn failed_snapshot_read_does_not_delete_unlisted_spool() {
    let root = Root::new();
    let spool = root.0.join("spool");
    fs::create_dir_all(&spool).unwrap();
    fs::write(spool.join("kept"), b"original").unwrap();
    let xml = "<ListBucketResult><IsTruncated>false</IsTruncated><Contents><Key>new</Key></Contents></ListBucketResult>";
    let (endpoint, done) = fixture(vec![(200, xml.into()), (500, String::new())]);
    let manager = StorageManager::new(Arc::new(object(&endpoint)), &spool).unwrap();
    assert!(manager.sync_all_to_spool().is_err());
    assert_eq!(fs::read(spool.join("kept")).unwrap(), b"original");
    done.recv_timeout(Duration::from_secs(10)).unwrap();
}

#[test]
fn reconstructed_object_store_reads_remote_bytes_and_empty_inventory() {
    let (endpoint, done) = fixture(vec![
        (200, String::new()),
        (200, "durable".into()),
        (204, String::new()),
        (404, String::new()),
        (
            200,
            "<ListBucketResult><IsTruncated>false</IsTruncated></ListBucketResult>".into(),
        ),
    ]);
    object(&endpoint).put("auths/a.json", b"durable").unwrap();
    let reopened = object(&endpoint);
    assert_eq!(reopened.get("auths/a.json").unwrap().unwrap(), b"durable");
    reopened.delete("auths/a.json").unwrap();
    assert!(object(&endpoint).get("auths/a.json").unwrap().is_none());
    assert!(reopened.list("").unwrap().is_empty());
    assert_eq!(done.recv_timeout(Duration::from_secs(10)).unwrap().len(), 5);
}

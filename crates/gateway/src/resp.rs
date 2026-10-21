use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::management::gate::ManagementAttempts;
use crate::management::store::SettingsStore;
use crate::server::ProtocolStream;

pub const REDIS_USAGE_CHANNEL: &str = "usage";
pub const REDIS_ERRORS_CHANNEL: &str = "errors";

pub const USAGE_SUPPORT_REFRESH_PAYLOAD: &str = "{\"support_refresh\":true}";
pub const USAGE_REFRESH_PAYLOAD: &str = "{\"refresh\":true}";

const DEFAULT_RETENTION_SECS: i64 = 60;
const MAX_RETENTION_SECS: i64 = 3600;
const USAGE_SUBSCRIBER_BUFFER: usize = 256;
const ERROR_SUBSCRIBER_BUFFER: usize = 256;

#[derive(Clone, Debug)]
struct QueueItem {
    enqueued_at: Instant,
    payload: Bytes,
}

#[derive(Default)]
struct Queue {
    items: Vec<QueueItem>,
    head: usize,
    subscribers: HashMap<u64, mpsc::Sender<Bytes>>,
    next_subscriber_id: u64,
}

impl Queue {
    fn clear(&mut self) {
        self.items.clear();
        self.head = 0;
        self.subscribers.clear();
    }

    fn prune(&mut self, now: Instant, retention: Duration) {
        if self.head >= self.items.len() {
            self.items.clear();
            self.head = 0;
            return;
        }
        let cutoff = now.checked_sub(retention).unwrap_or(now);
        while self.head < self.items.len() && self.items[self.head].enqueued_at < cutoff {
            self.head += 1;
        }
    }

    fn maybe_compact(&mut self) {
        if self.head == 0 {
            return;
        }
        if self.head >= self.items.len() {
            self.items.clear();
            self.head = 0;
            return;
        }
        if self.head < 1024 && self.head * 2 < self.items.len() {
            return;
        }
        self.items.drain(0..self.head);
        self.head = 0;
    }

    fn len(&mut self, now: Instant, retention: Duration) -> usize {
        self.prune(now, retention);
        self.items.len().saturating_sub(self.head)
    }

    fn enqueue(&mut self, payload: Bytes, now: Instant, retention: Duration) {
        self.prune(now, retention);
        self.items.push(QueueItem {
            enqueued_at: now,
            payload,
        });
        self.maybe_compact();
    }

    fn publish_to_subscribers(&mut self, payload: &Bytes) -> bool {
        if self.subscribers.is_empty() {
            return false;
        }
        let mut disconnected = Vec::new();
        for (&id, sender) in &self.subscribers {
            match sender.try_send(payload.clone()) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_))
                | Err(mpsc::error::TrySendError::Closed(_)) => {
                    disconnected.push(id);
                }
            }
        }
        for id in disconnected {
            self.subscribers.remove(&id);
        }
        true
    }

    fn subscribe(
        &mut self,
        buffer: usize,
        initial_payload: Option<Bytes>,
    ) -> (mpsc::Receiver<Bytes>, u64) {
        let (tx, rx) = mpsc::channel(buffer);
        if let Some(initial) = initial_payload {
            let _ = tx.try_send(initial);
        }
        self.next_subscriber_id += 1;
        let id = self.next_subscriber_id;
        self.subscribers.insert(id, tx);
        (rx, id)
    }

    fn unsubscribe(&mut self, id: u64) {
        self.subscribers.remove(&id);
    }

    fn pop_oldest(&mut self, count: usize, now: Instant, retention: Duration) -> Vec<Bytes> {
        self.prune(now, retention);
        let available = self.items.len().saturating_sub(self.head);
        if available == 0 || count == 0 {
            return Vec::new();
        }
        let pop_count = count.min(available);
        let mut out = Vec::with_capacity(pop_count);
        for i in 0..pop_count {
            out.push(self.items[self.head + i].payload.clone());
        }
        self.head += pop_count;
        self.maybe_compact();
        out
    }
}

pub struct RedisQueueHub {
    enabled: AtomicBool,
    retention_secs: AtomicI64,
    usage_statistics_enabled: AtomicBool,
    global_usage: Arc<Mutex<Queue>>,
    global_errors: Arc<Mutex<Queue>>,
}

impl Default for RedisQueueHub {
    fn default() -> Self {
        Self {
            enabled: AtomicBool::new(true),
            retention_secs: AtomicI64::new(DEFAULT_RETENTION_SECS),
            usage_statistics_enabled: AtomicBool::new(true),
            global_usage: Arc::new(Mutex::new(Queue::default())),
            global_errors: Arc::new(Mutex::new(Queue::default())),
        }
    }
}

static REDIS_HUB: OnceLock<Arc<RedisQueueHub>> = OnceLock::new();

pub fn global_redis_hub() -> &'static Arc<RedisQueueHub> {
    REDIS_HUB.get_or_init(|| Arc::new(RedisQueueHub::default()))
}

impl RedisQueueHub {
    pub fn set_enabled(&self, value: bool) {
        self.enabled.store(value, Ordering::SeqCst);
        if !value {
            self.global_usage.lock().unwrap().clear();
            self.global_errors.lock().unwrap().clear();
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    pub fn set_usage_statistics_enabled(&self, value: bool) {
        self.usage_statistics_enabled
            .store(value, Ordering::Relaxed);
    }

    pub fn usage_statistics_enabled(&self) -> bool {
        self.usage_statistics_enabled.load(Ordering::Relaxed)
    }

    pub fn set_retention_secs(&self, value: i64) {
        let normalized = if value <= 0 {
            DEFAULT_RETENTION_SECS
        } else if value > MAX_RETENTION_SECS {
            MAX_RETENTION_SECS
        } else {
            value
        };
        self.retention_secs.store(normalized, Ordering::Relaxed);
    }

    pub fn retention_secs(&self) -> i64 {
        self.retention_secs.load(Ordering::Relaxed)
    }

    fn retention_duration(&self) -> Duration {
        Duration::from_secs(self.retention_secs().max(1) as u64)
    }

    pub fn enqueue_usage(&self, payload: Bytes) {
        if !self.enabled() || !self.usage_statistics_enabled() || payload.is_empty() {
            return;
        }
        let mut q = self.global_usage.lock().unwrap();
        if q.publish_to_subscribers(&payload) {
            return;
        }
        let now = Instant::now();
        let retention = self.retention_duration();
        q.enqueue(payload, now, retention);
    }

    pub fn enqueue_error(&self, payload: Bytes) {
        if !self.enabled() || payload.is_empty() {
            return;
        }
        let mut q = self.global_errors.lock().unwrap();
        q.publish_to_subscribers(&payload);
    }

    pub fn pop_oldest_usage(&self, count: usize) -> Vec<Bytes> {
        if !self.enabled() || count == 0 {
            return Vec::new();
        }
        let mut q = self.global_usage.lock().unwrap();
        let now = Instant::now();
        let retention = self.retention_duration();
        q.pop_oldest(count, now, retention)
    }

    pub fn queue_len(&self, channel: &str) -> Option<usize> {
        if !self.enabled() {
            return Some(0);
        }
        match channel.to_ascii_lowercase().trim() {
            REDIS_USAGE_CHANNEL => {
                let mut q = self.global_usage.lock().unwrap();
                let now = Instant::now();
                let retention = self.retention_duration();
                Some(q.len(now, retention))
            }
            REDIS_ERRORS_CHANNEL => Some(0),
            _ => None,
        }
    }

    pub fn subscribe_usage(&self) -> (mpsc::Receiver<Bytes>, Box<dyn FnOnce() + Send + 'static>) {
        let mut q = self.global_usage.lock().unwrap();
        let initial = Bytes::from_static(USAGE_SUPPORT_REFRESH_PAYLOAD.as_bytes());
        let (rx, id) = q.subscribe(USAGE_SUBSCRIBER_BUFFER, Some(initial));
        let queue = Arc::clone(&self.global_usage);
        let unsub = Box::new(move || {
            let mut q = queue.lock().unwrap();
            q.unsubscribe(id);
        });
        (rx, unsub)
    }

    pub fn subscribe_errors(&self) -> (mpsc::Receiver<Bytes>, Box<dyn FnOnce() + Send + 'static>) {
        let mut q = self.global_errors.lock().unwrap();
        let (rx, id) = q.subscribe(ERROR_SUBSCRIBER_BUFFER, None);
        let queue = Arc::clone(&self.global_errors);
        let unsub = Box::new(move || {
            let mut q = queue.lock().unwrap();
            q.unsubscribe(id);
        });
        (rx, unsub)
    }

    pub fn notify_usage_refresh(&self) {
        if !self.enabled() {
            return;
        }
        let mut q = self.global_usage.lock().unwrap();
        let payload = Bytes::from_static(USAGE_REFRESH_PAYLOAD.as_bytes());
        q.publish_to_subscribers(&payload);
    }
}

pub async fn read_resp_array(stream: &mut ProtocolStream) -> Result<Vec<String>, String> {
    let prefix = match stream.read_u8().await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err("EOF".to_string()),
        Err(e) => return Err(e.to_string()),
    };

    if prefix != b'*' {
        return Err("protocol error".to_string());
    }

    let line = read_resp_line(stream).await?;
    let count: i64 = line.parse().map_err(|_| "protocol error".to_string())?;
    if count < 0 {
        return Err("protocol error".to_string());
    }

    let mut args = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let arg = read_resp_string(stream).await?;
        args.push(arg);
    }
    Ok(args)
}

async fn read_resp_string(stream: &mut ProtocolStream) -> Result<String, String> {
    let prefix = stream.read_u8().await.map_err(|e| e.to_string())?;
    match prefix {
        b'$' => read_resp_bulk_string(stream).await,
        b'+' | b':' => read_resp_line(stream).await,
        _ => Err("protocol error".to_string()),
    }
}

async fn read_resp_bulk_string(stream: &mut ProtocolStream) -> Result<String, String> {
    let line = read_resp_line(stream).await?;
    let len: i64 = line.parse().map_err(|_| "protocol error".to_string())?;
    if len < 0 {
        return Ok(String::new());
    }
    let usize_len = len as usize;
    let mut buf = vec![0u8; usize_len + 2];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| e.to_string())?;
    if buf[usize_len] != b'\r' || buf[usize_len + 1] != b'\n' {
        return Err("protocol error".to_string());
    }
    String::from_utf8(buf[..usize_len].to_vec())
        .map_err(|_| "protocol error: invalid utf-8".to_string())
}

async fn read_resp_line(stream: &mut ProtocolStream) -> Result<String, String> {
    let mut line = String::new();
    stream
        .read_line(&mut line)
        .await
        .map_err(|e| e.to_string())?;
    if line.ends_with("\r\n") {
        line.truncate(line.len() - 2);
    } else if line.ends_with('\n') {
        line.truncate(line.len() - 1);
    }
    Ok(line)
}

pub async fn write_simple_string(stream: &mut ProtocolStream, s: &str) -> std::io::Result<()> {
    stream.write_all(format!("+{s}\r\n").as_bytes()).await?;
    stream.flush().await
}

pub async fn write_error(stream: &mut ProtocolStream, msg: &str) -> std::io::Result<()> {
    stream.write_all(format!("-{msg}\r\n").as_bytes()).await?;
    stream.flush().await
}

pub async fn write_integer(stream: &mut ProtocolStream, val: i64) -> std::io::Result<()> {
    stream.write_all(format!(":{val}\r\n").as_bytes()).await?;
    stream.flush().await
}

pub async fn write_nil_bulk_string(stream: &mut ProtocolStream) -> std::io::Result<()> {
    stream.write_all(b"$-1\r\n").await?;
    stream.flush().await
}

pub async fn write_bulk_string(stream: &mut ProtocolStream, data: &[u8]) -> std::io::Result<()> {
    stream
        .write_all(format!("${}\r\n", data.len()).as_bytes())
        .await?;
    stream.write_all(data).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await
}

pub async fn write_array_of_bulk_strings(
    stream: &mut ProtocolStream,
    items: &[Bytes],
) -> std::io::Result<()> {
    stream
        .write_all(format!("*{}\r\n", items.len()).as_bytes())
        .await?;
    for item in items {
        stream
            .write_all(format!("${}\r\n", item.len()).as_bytes())
            .await?;
        stream.write_all(item).await?;
        stream.write_all(b"\r\n").await?;
    }
    stream.flush().await
}

pub async fn write_pubsub_subscribe(
    stream: &mut ProtocolStream,
    channel: &str,
    count: usize,
) -> std::io::Result<()> {
    stream.write_all(b"*3\r\n$9\r\nsubscribe\r\n").await?;
    stream
        .write_all(format!("${}\r\n{}\r\n", channel.len(), channel).as_bytes())
        .await?;
    stream.write_all(format!(":{count}\r\n").as_bytes()).await?;
    stream.flush().await
}

pub async fn write_pubsub_unsubscribe(
    stream: &mut ProtocolStream,
    channel: &str,
    count: usize,
) -> std::io::Result<()> {
    stream.write_all(b"*3\r\n$11\r\nunsubscribe\r\n").await?;
    stream
        .write_all(format!("${}\r\n{}\r\n", channel.len(), channel).as_bytes())
        .await?;
    stream.write_all(format!(":{count}\r\n").as_bytes()).await?;
    stream.flush().await
}

pub async fn write_pubsub_message(
    stream: &mut ProtocolStream,
    channel: &str,
    payload: &[u8],
) -> std::io::Result<()> {
    stream.write_all(b"*3\r\n$7\r\nmessage\r\n").await?;
    stream
        .write_all(format!("${}\r\n{}\r\n", channel.len(), channel).as_bytes())
        .await?;
    stream
        .write_all(format!("${}\r\n", payload.len()).as_bytes())
        .await?;
    stream.write_all(payload).await?;
    stream.write_all(b"\r\n").await?;
    stream.flush().await
}

pub async fn write_pubsub_pong(
    stream: &mut ProtocolStream,
    payload: Option<&[u8]>,
) -> std::io::Result<()> {
    stream.write_all(b"*2\r\n$4\r\npong\r\n").await?;
    if let Some(data) = payload {
        stream
            .write_all(format!("${}\r\n", data.len()).as_bytes())
            .await?;
        stream.write_all(data).await?;
        stream.write_all(b"\r\n").await?;
    } else {
        stream.write_all(b"$0\r\n\r\n").await?;
    }
    stream.flush().await
}

fn check_management_auth(
    settings: Option<&SettingsStore>,
    attempts: &Mutex<ManagementAttempts>,
    peer: SocketAddr,
    password: &str,
) -> Result<(), (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;
    let configured = settings.map(|s| s.current());
    let secret = configured
        .as_ref()
        .map(|s| s.remote_management.secret_key.as_str())
        .unwrap_or("");
    let env_secret = std::env::var("MANAGEMENT_PASSWORD").unwrap_or_default();

    if secret.is_empty() && env_secret.is_empty() {
        return Err((
            StatusCode::NOT_FOUND,
            "remote management disabled".to_string(),
        ));
    }

    let ip = peer.ip();
    let allow_remote = !env_secret.is_empty()
        || configured
            .as_ref()
            .is_some_and(|s| s.remote_management.allow_remote);
    let missing = password.is_empty();

    let valid = if missing {
        false
    } else if !env_secret.is_empty() && constant_time_eq(&env_secret, password) {
        true
    } else if secret.starts_with("$2") {
        bcrypt::verify(password, secret).unwrap_or(false)
    } else {
        !secret.is_empty() && constant_time_eq(secret, password)
    };

    let mut lock = attempts.lock().unwrap_or_else(|p| p.into_inner());
    lock.authenticate(ip, allow_remote, valid, missing, Instant::now())
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn parse_auth_password(args: &[String]) -> Option<String> {
    match args.len() {
        2 => Some(args[1].clone()),
        3 => Some(args[2].clone()),
        _ => None,
    }
}

fn parse_pop_count(args: &[String]) -> Result<(usize, bool), String> {
    if args.len() != 2 && args.len() != 3 {
        return Err("wrong number of arguments".to_string());
    }
    if args.len() == 2 {
        return Ok((1, false));
    }
    let count: i64 = args[2]
        .trim()
        .parse()
        .map_err(|_| "value is not an integer or out of range".to_string())?;
    if count <= 0 {
        return Err("value is not an integer or out of range".to_string());
    }
    Ok((count as usize, true))
}

pub async fn handle_redis_connection(
    mut stream: ProtocolStream,
    peer: SocketAddr,
    settings: Option<Arc<SettingsStore>>,
    attempts: Arc<Mutex<ManagementAttempts>>,
    hub: Arc<RedisQueueHub>,
) {
    let mut authed = false;

    if let Some(ref store) = settings {
        let current = store.current();
        if current.home.enabled {
            let _ = write_error(&mut stream, "ERR redis usage output disabled in home mode").await;
            return;
        }
    }

    loop {
        let args = match read_resp_array(&mut stream).await {
            Ok(args) => args,
            Err(e) => {
                if e != "EOF" {
                    let _ = write_error(&mut stream, &format!("ERR {e}")).await;
                }
                return;
            }
        };

        if args.is_empty() {
            if write_error(&mut stream, "ERR empty command").await.is_err() {
                return;
            }
            continue;
        }

        let cmd = args[0].to_ascii_uppercase();

        if cmd != "AUTH" && !authed {
            let auth_res = check_management_auth(settings.as_deref(), &attempts, peer, "");
            if let Err((status, msg)) = auth_res {
                if status == axum::http::StatusCode::FORBIDDEN && msg.starts_with("IP banned") {
                    let _ = write_error(&mut stream, &format!("ERR {msg}")).await;
                } else {
                    let _ = write_error(&mut stream, "NOAUTH Authentication required.").await;
                }
            } else {
                let _ = write_error(&mut stream, "NOAUTH Authentication required.").await;
            }
            continue;
        }

        match cmd.as_str() {
            "AUTH" => {
                let Some(password) = parse_auth_password(&args) else {
                    let auth_res = check_management_auth(settings.as_deref(), &attempts, peer, "");
                    if let Err((status, msg)) = auth_res {
                        if status == axum::http::StatusCode::FORBIDDEN
                            && msg.starts_with("IP banned")
                        {
                            let _ = write_error(&mut stream, &format!("ERR {msg}")).await;
                            continue;
                        }
                    }
                    let _ = write_error(
                        &mut stream,
                        "ERR wrong number of arguments for 'auth' command",
                    )
                    .await;
                    continue;
                };

                match check_management_auth(settings.as_deref(), &attempts, peer, &password) {
                    Ok(()) => {
                        authed = true;
                        if write_simple_string(&mut stream, "OK").await.is_err() {
                            return;
                        }
                    }
                    Err((_, err_msg)) => {
                        if write_error(&mut stream, &format!("ERR {err_msg}"))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            "PING" => {
                let payload = if args.len() > 1 {
                    Some(args[1].as_str())
                } else {
                    None
                };
                let res = match payload {
                    Some(msg) => write_bulk_string(&mut stream, msg.as_bytes()).await,
                    None => write_simple_string(&mut stream, "PONG").await,
                };
                if res.is_err() {
                    return;
                }
            }
            "LLEN" => {
                if args.len() != 2 {
                    if write_error(
                        &mut stream,
                        "ERR wrong number of arguments for 'llen' command",
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                    continue;
                }
                let channel = &args[1];
                match hub.queue_len(channel) {
                    Some(len) => {
                        if write_integer(&mut stream, len as i64).await.is_err() {
                            return;
                        }
                    }
                    None => {
                        if write_error(&mut stream, &format!("ERR unsupported channel '{channel}'"))
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }
            }
            "LPOP" | "RPOP" => {
                let (count, has_count) = match parse_pop_count(&args) {
                    Ok(res) => res,
                    Err(err) => {
                        let _ = write_error(&mut stream, &format!("ERR {err}")).await;
                        continue;
                    }
                };
                let channel = args[1].to_ascii_lowercase();
                if channel != REDIS_USAGE_CHANNEL {
                    let _ = write_error(
                        &mut stream,
                        &format!("ERR unsupported channel '{}'", args[1]),
                    )
                    .await;
                    continue;
                }
                let items = hub.pop_oldest_usage(count);
                if has_count {
                    if write_array_of_bulk_strings(&mut stream, &items)
                        .await
                        .is_err()
                    {
                        return;
                    }
                } else if items.is_empty() {
                    if write_nil_bulk_string(&mut stream).await.is_err() {
                        return;
                    }
                } else if write_bulk_string(&mut stream, &items[0]).await.is_err() {
                    return;
                }
            }
            "SUBSCRIBE" => {
                if args.len() != 2 {
                    if write_error(
                        &mut stream,
                        "ERR wrong number of arguments for 'subscribe' command",
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                    continue;
                }
                let channel = args[1].trim().to_string();
                let channel_lower = channel.to_ascii_lowercase();
                let (rx, unsub) = if channel_lower == REDIS_USAGE_CHANNEL {
                    hub.subscribe_usage()
                } else if channel_lower == REDIS_ERRORS_CHANNEL {
                    hub.subscribe_errors()
                } else {
                    let _ =
                        write_error(&mut stream, &format!("ERR unsupported channel '{channel}'"))
                            .await;
                    continue;
                };

                if write_pubsub_subscribe(&mut stream, &channel, 1)
                    .await
                    .is_err()
                {
                    unsub();
                    return;
                }

                stream_subscription(&mut stream, &channel, rx, unsub).await;
                return;
            }
            "QUIT" => {
                let _ = write_simple_string(&mut stream, "OK").await;
                return;
            }
            _ => {
                if write_error(
                    &mut stream,
                    &format!("ERR unknown command '{}'", cmd.to_ascii_lowercase()),
                )
                .await
                .is_err()
                {
                    return;
                }
            }
        }
    }
}

async fn stream_subscription(
    stream: &mut ProtocolStream,
    channel: &str,
    mut messages: mpsc::Receiver<Bytes>,
    unsubscribe: Box<dyn FnOnce() + Send + 'static>,
) {
    let mut unsubscribe = Some(unsubscribe);
    let _unsubscribe_guard = defer_unsubscribe(&mut unsubscribe);

    loop {
        tokio::select! {
            msg = messages.recv() => {
                match msg {
                    Some(payload) => {
                        if write_pubsub_message(stream, channel, &payload).await.is_err() {
                            return;
                        }
                    }
                    None => return,
                }
            }
            read_res = read_resp_array(stream) => {
                match read_res {
                    Ok(args) => {
                        if args.is_empty() {
                            let _ = write_error(stream, "ERR empty command").await;
                            continue;
                        }
                        let sub_cmd = args[0].to_ascii_uppercase();
                        match sub_cmd.as_str() {
                            "PING" => {
                                let payload = if args.len() > 1 {
                                    Some(args[1].as_bytes())
                                } else {
                                    None
                                };
                                if write_pubsub_pong(stream, payload).await.is_err() {
                                    return;
                                }
                            }
                            "UNSUBSCRIBE" => {
                                let _ = write_pubsub_unsubscribe(stream, channel, 0).await;
                                return;
                            }
                            "QUIT" => {
                                let _ = write_simple_string(stream, "OK").await;
                                return;
                            }
                            _ => {
                                if write_error(stream, &format!("ERR unknown command '{}'", sub_cmd.to_ascii_lowercase())).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        if e != "EOF" {
                            let _ = write_error(stream, &format!("ERR {e}")).await;
                        }
                        return;
                    }
                }
            }
        }
    }
}

fn defer_unsubscribe(unsub: &mut Option<Box<dyn FnOnce() + Send + 'static>>) -> impl Drop {
    struct Guard(Option<Box<dyn FnOnce() + Send + 'static>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(fn_once) = self.0.take() {
                fn_once();
            }
        }
    }
    Guard(unsub.take())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_hub_queue_enqueue_pop_and_broadcast() {
        let hub = RedisQueueHub::default();
        let (mut sub1, unsub1) = hub.subscribe_usage();
        let (mut sub2, unsub2) = hub.subscribe_usage();

        let init1 = sub1.recv().await.unwrap();
        assert_eq!(init1.as_ref(), USAGE_SUPPORT_REFRESH_PAYLOAD.as_bytes());
        let init2 = sub2.recv().await.unwrap();
        assert_eq!(init2.as_ref(), USAGE_SUPPORT_REFRESH_PAYLOAD.as_bytes());

        hub.enqueue_usage(Bytes::from_static(b"record-1"));
        assert_eq!(sub1.recv().await.unwrap().as_ref(), b"record-1");
        assert_eq!(sub2.recv().await.unwrap().as_ref(), b"record-1");
        assert_eq!(hub.pop_oldest_usage(1).len(), 0);

        unsub1();
        unsub2();

        hub.enqueue_usage(Bytes::from_static(b"queued-record-1"));
        hub.enqueue_usage(Bytes::from_static(b"queued-record-2"));
        assert_eq!(hub.queue_len(REDIS_USAGE_CHANNEL), Some(2));

        let popped = hub.pop_oldest_usage(10);
        assert_eq!(popped.len(), 2);
        assert_eq!(popped[0].as_ref(), b"queued-record-1");
        assert_eq!(popped[1].as_ref(), b"queued-record-2");
        assert_eq!(hub.queue_len(REDIS_USAGE_CHANNEL), Some(0));
    }

    #[tokio::test]
    async fn test_error_channel_broadcasts_and_discards_without_subscribers() {
        let hub = RedisQueueHub::default();
        let (mut sub, unsub) = hub.subscribe_errors();

        hub.enqueue_error(Bytes::from_static(b"err-1"));
        assert_eq!(sub.recv().await.unwrap().as_ref(), b"err-1");

        unsub();
        hub.enqueue_error(Bytes::from_static(b"discarded-err"));
        assert_eq!(hub.queue_len(REDIS_ERRORS_CHANNEL), Some(0));
    }

    #[tokio::test]
    async fn test_resp_serialization_and_parsing() {
        let (client, server) = tokio::io::duplex(1024);
        let mut server_reader: ProtocolStream = tokio::io::BufReader::new(Box::new(server));

        tokio::spawn(async move {
            let mut client = client;
            client
                .write_all(b"*2\r\n$4\r\nAUTH\r\n$8\r\npassword\r\n")
                .await
                .unwrap();
            client.flush().await.unwrap();
        });

        let args = read_resp_array(&mut server_reader).await.unwrap();
        assert_eq!(args, vec!["AUTH", "password"]);
    }
}

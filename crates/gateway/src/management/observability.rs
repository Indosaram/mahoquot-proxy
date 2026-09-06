use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures::stream::Stream;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use super::scalar_table::Refusal;
use super::{scalars, settings::Settings};
use crate::state::AppState;

fn json_status(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// Upstream serves log files out of a directory beside the config. While
/// `logging-to-file` is off the file-backed routes keep refusing with 400 so a
/// client can tell "disabled" from "no logs"; `/logs` itself always answers,
/// falling back to the in-memory tail (see `LogTail`).
fn log_dir(settings: &Settings) -> std::path::PathBuf {
    std::path::PathBuf::from(&settings.auth_dir).join("logs")
}

fn require_file_logging(settings: &Settings) -> Option<Response> {
    if settings.logging_to_file {
        return None;
    }
    Some(json_status(
        StatusCode::BAD_REQUEST,
        json!({ "error": "logging to file disabled" }),
    ))
}

/// Capacity of the in-memory tail served while file logging is off.
const LOG_TAIL_CAPACITY: usize = 1000;

/// Records returned by `/logs` when the caller does not ask for a size. The
/// console renders a live tail, so an unbounded reply would re-send the whole
/// on-disk history on every poll.
const DEFAULT_LOG_LIMIT: usize = 500;

/// Buffered lines per live subscriber before a slow reader is told it lagged.
const LOG_STREAM_BUFFER: usize = 256;

/// Bounded in-memory tail of recent log lines. File persistence is a setting;
/// the live tail is always fed, so the Logs surface keeps showing real-time
/// output even while `logging-to-file` is off.
pub struct LogTail {
    lines: Mutex<VecDeque<String>>,
    live: broadcast::Sender<String>,
}

impl Default for LogTail {
    fn default() -> Self {
        Self {
            lines: Mutex::default(),
            live: broadcast::channel(LOG_STREAM_BUFFER).0,
        }
    }
}

impl LogTail {
    pub fn push(&self, line: String) {
        {
            let mut lines = self.lines.lock().expect("log tail lock");
            if lines.len() >= LOG_TAIL_CAPACITY {
                lines.pop_front();
            }
            lines.push_back(line.clone());
        }
        // No subscriber is the normal case; the error only says nobody listened.
        let _ = self.live.send(line);
    }

    /// Live feed of lines appended after subscription.
    pub fn subscribe(&self) -> broadcast::Receiver<String> {
        self.live.subscribe()
    }

    pub fn snapshot(&self) -> Vec<String> {
        let lines = self.lines.lock().expect("log tail lock");
        lines.iter().cloned().collect()
    }

    pub fn clear(&self) {
        self.lines.lock().expect("log tail lock").clear();
    }
}

pub fn append_log_line(settings: &Settings, line: &str) {
    if !settings.logging_to_file {
        return;
    }
    let dir = log_dir(settings);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    use std::io::Write;
    let path = dir.join("gateway.log");
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(file, "{line}");
    }
    rotate_log_segments(
        &dir,
        settings.logs_segment_size_mb.max(0) as u64 * 1024 * 1024,
        settings.logs_max_total_size_mb.max(0) as u64 * 1024 * 1024,
    );
}

/// Active log file; rotated segments are `gateway.log.<n>` with a higher `n`
/// meaning newer.
const ACTIVE_LOG: &str = "gateway.log";

/// Sequence number of a rotated segment, or `None` for anything else
/// (including the active file).
fn segment_ordinal(name: &str) -> Option<u64> {
    name.strip_prefix(ACTIVE_LOG)?
        .strip_prefix('.')?
        .parse::<u64>()
        .ok()
}

/// Rotated segments plus their sizes, oldest first.
fn log_segments(dir: &std::path::Path) -> Vec<(u64, std::path::PathBuf, u64)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut segments: Vec<(u64, std::path::PathBuf, u64)> = entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            let ordinal = segment_ordinal(&entry.file_name().to_string_lossy())?;
            Some((ordinal, entry.path(), meta.len()))
        })
        .collect();
    segments.sort_by_key(|(ordinal, _, _)| *ordinal);
    segments
}

/// Roll the active file into a numbered segment once it reaches
/// `segment_bytes`, then delete the oldest segments until the directory fits
/// `max_total_bytes`. Renaming and unlinking keep this O(1) in file size: the
/// previous strategy rewrote the whole capped file, which at a multi-hundred
/// megabyte cap meant reading and writing it back on every trim.
fn rotate_log_segments(dir: &std::path::Path, segment_bytes: u64, max_total_bytes: u64) {
    let active = dir.join(ACTIVE_LOG);
    if segment_bytes > 0 {
        let active_size = std::fs::metadata(&active).map(|m| m.len()).unwrap_or(0);
        if active_size >= segment_bytes {
            let next = log_segments(dir)
                .last()
                .map(|(ordinal, _, _)| ordinal + 1)
                .unwrap_or(1);
            // A failed rename must not lose lines: keep appending to the
            // active file and retry on the next write.
            let _ = std::fs::rename(&active, dir.join(format!("{ACTIVE_LOG}.{next:03}")));
        }
    }
    if max_total_bytes == 0 {
        return;
    }
    let mut segments = log_segments(dir);
    let active_size = std::fs::metadata(&active).map(|m| m.len()).unwrap_or(0);
    let mut total: u64 = active_size + segments.iter().map(|(_, _, size)| size).sum::<u64>();
    // Oldest first: the active file is never dropped, so a cap smaller than one
    // segment still keeps the newest lines.
    for (_, path, size) in segments.drain(..) {
        if total <= max_total_bytes {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

fn read_log_lines(dir: &std::path::Path) -> Vec<String> {
    // Oldest segment first, active file last, so the newest line is the tail.
    // A plain path sort would order `gateway.log.10` before `gateway.log.9`
    // and put the active file ahead of every segment.
    let mut paths: Vec<std::path::PathBuf> = log_segments(dir)
        .into_iter()
        .map(|(_, path, _)| path)
        .collect();
    let active = dir.join(ACTIVE_LOG);
    if active.is_file() {
        paths.push(active);
    }
    paths
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|body| body.lines().map(str::to_string).collect::<Vec<_>>())
        .collect()
}

fn list_log_files(dir: &std::path::Path) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<Value> = entries
        .flatten()
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            if !meta.is_file() {
                return None;
            }
            Some(json!({
                "name": entry.file_name().to_string_lossy(),
                "size": meta.len(),
            }))
        })
        .collect();
    files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    files
}

/// Parse one stored log line into a structured record. Well-formed records
/// pass through; legacy or foreign lines degrade to proxy events so the UI
/// never loses them.
fn parse_log_record(line: &str) -> Value {
    match serde_json::from_str::<Value>(line) {
        Ok(value) if value.get("kind").is_some() => value,
        _ => json!({ "kind": "proxy", "timestamp": Value::Null, "message": line }),
    }
}

/// `limit=0` means "every retained line"; omitting it keeps the reply bounded.
#[derive(Debug, Default, Deserialize)]
struct LogsQuery {
    limit: Option<usize>,
}

async fn get_logs(State(state): State<Arc<AppState>>, Query(query): Query<LogsQuery>) -> Response {
    let settings = state.settings.current();
    let dir = log_dir(&settings);
    let mut lines = if settings.logging_to_file {
        // Every retained segment is read whole, so this grows with the log cap
        // and must not run on an executor thread.
        match tokio::task::spawn_blocking(move || read_log_lines(&dir)).await {
            Ok(lines) => lines,
            Err(err) => {
                tracing::error!("log read task failed: {err}");
                Vec::new()
            }
        }
    } else {
        // File logging is off, but the live tail is still being fed: answer
        // with it instead of refusing, so the Logs surface shows real-time
        // output rather than an error.
        state.log_tail.snapshot()
    };
    let retained = lines.len();
    let limit = query.limit.unwrap_or(DEFAULT_LOG_LIMIT);
    if limit > 0 && retained > limit {
        // Keep the newest lines: the console renders a tail, not an archive.
        lines.drain(..retained - limit);
    }
    let records: Vec<Value> = lines.iter().map(|line| parse_log_record(line)).collect();
    let request_count = records
        .iter()
        .filter(|record| record["kind"] == "request")
        .count();
    json_status(
        StatusCode::OK,
        json!({
            "records": records,
            "request-count": request_count,
            "proxy-count": records.len() - request_count,
            "retained-count": retained,
            "latest-timestamp": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or_default(),
        }),
    )
}

async fn delete_logs(State(state): State<Arc<AppState>>) -> Response {
    let settings = state.settings.current();
    // The tail is the live data while file logging is off, so a clear must
    // always reach it; files are only touched while logging is enabled.
    state.log_tail.clear();
    let dir = log_dir(&settings);
    let mut removed = 0u64;
    if settings.logging_to_file {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if entry.metadata().map(|m| m.is_file()).unwrap_or(false)
                    && std::fs::remove_file(entry.path()).is_ok()
                {
                    removed += 1;
                }
            }
        }
    }
    json_status(
        StatusCode::OK,
        json!({ "success": true, "removed": removed, "message": "Logs cleared successfully" }),
    )
}

async fn request_error_logs(State(state): State<Arc<AppState>>) -> Response {
    let settings = state.settings.current();
    if let Some(refusal) = require_file_logging(&settings) {
        return refusal;
    }
    json_status(
        StatusCode::OK,
        json!({ "files": list_log_files(&log_dir(&settings)) }),
    )
}

async fn request_error_log_by_name(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let settings = state.settings.current();
    if let Some(refusal) = require_file_logging(&settings) {
        return refusal;
    }
    if name.contains('/') || name.contains("..") {
        return json_status(StatusCode::BAD_REQUEST, json!({ "error": "invalid name" }));
    }
    match std::fs::read_to_string(log_dir(&settings).join(&name)) {
        Ok(body) => (StatusCode::OK, body).into_response(),
        Err(_) => json_status(StatusCode::NOT_FOUND, json!({ "error": "not found" })),
    }
}

async fn request_log_by_id(Path(id): Path<String>) -> Response {
    json_status(
        StatusCode::NOT_FOUND,
        json!({ "error": "not found", "id": id }),
    )
}

/// Push newly appended log lines to the console so it never has to poll.
async fn stream_logs(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let receiver = state.log_tail.subscribe();
    let stream = futures::stream::unfold(receiver, |mut receiver| async move {
        loop {
            match receiver.recv().await {
                // Stream the same record shape `/logs` returns, so a client
                // parses one schema instead of two.
                Ok(line) => {
                    let record = parse_log_record(&line).to_string();
                    return Some((Ok(Event::default().data(record)), receiver));
                }
                // A lagging reader loses the skipped lines, not the stream.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

pub fn observability_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/logs", get(get_logs).delete(delete_logs))
        .route("/logs/stream", get(stream_logs))
        .route("/request-error-logs", get(request_error_logs))
        .route("/request-error-logs/{name}", get(request_error_log_by_name))
        .route("/request-log-by-id/{id}", get(request_log_by_id))
        .route(
            "/request-log",
            get(|State(state): State<Arc<AppState>>| async move {
                let value = state.settings.current().request_log;
                json_status(StatusCode::OK, json!({ "request-log": value }))
            })
            .put(write_request_log)
            .patch(write_request_log),
        )
        .route(
            "/logs-max-total-size-mb",
            get(|State(state): State<Arc<AppState>>| async move {
                let value = state.settings.current().logs_max_total_size_mb;
                json_status(StatusCode::OK, json!({ "logs-max-total-size-mb": value }))
            })
            .post(write_logs_cap)
            .put(write_logs_cap)
            .patch(write_logs_cap),
        )
}

async fn write_request_log(State(state): State<Arc<AppState>>, raw: bytes::Bytes) -> Response {
    write_field(state, raw, |settings, value| {
        settings.request_log = value.as_bool().ok_or(Refusal::InvalidBody)?;
        Ok(())
    }).await
}

async fn write_logs_cap(State(state): State<Arc<AppState>>, raw: bytes::Bytes) -> Response {
    write_field(state, raw, |settings, value| {
        settings.logs_max_total_size_mb = value.as_i64().ok_or(Refusal::InvalidBody)?;
        Ok(())
    }).await
}

async fn write_field(
    state: Arc<AppState>,
    raw: bytes::Bytes,
    set: impl FnOnce(&mut Settings, &Value) -> Result<(), Refusal> + Send + 'static,
) -> Response {
    let Ok(body) = serde_json::from_slice::<Value>(&raw) else {
        return scalars::refusal_response(Refusal::InvalidBody);
    };
    let Some(value) = body.get("value").cloned() else {
        return scalars::refusal_response(Refusal::InvalidBody);
    };
    scalars::apply_edit(&state, move |settings| set(settings, &value)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_backed_log_routes_are_refused_while_file_logging_is_off() {
        // given a config with logging-to-file disabled
        let settings = Settings {
            logging_to_file: false,
            ..Settings::default()
        };
        // when a file-backed log route checks availability
        let refusal = require_file_logging(&settings);
        // then it refuses rather than reporting an empty list
        assert!(refusal.is_some());
    }

    #[test]
    fn log_tail_serves_recent_lines_in_order_and_stays_bounded() {
        // given a tail already at capacity
        let tail = LogTail::default();
        for index in 0..(LOG_TAIL_CAPACITY as u64) {
            tail.push(index.to_string());
        }
        // when one more line is pushed
        tail.push("newest".to_string());
        // then the oldest line was dropped and order is preserved
        let snapshot = tail.snapshot();
        assert_eq!(snapshot.len(), LOG_TAIL_CAPACITY);
        assert_eq!(snapshot[0], (1u64).to_string());
        assert_eq!(snapshot[LOG_TAIL_CAPACITY - 1], "newest");
        // and clearing empties it completely
        tail.clear();
        assert!(tail.snapshot().is_empty());
    }

    #[test]
    fn log_routes_are_available_once_file_logging_is_on() {
        // given logging enabled
        let settings = Settings {
            logging_to_file: true,
            ..Settings::default()
        };
        // then the guard lets the request through
        assert!(require_file_logging(&settings).is_none());
    }

    #[test]
    fn listing_reports_real_files_and_skips_directories() {
        // given a log directory holding a file and a subdirectory
        let dir = std::env::temp_dir().join(format!("mahoquot-logs-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("nested")).expect("dirs");
        std::fs::write(dir.join("app.log"), "hello").expect("write");
        // when listed
        let files = list_log_files(&dir);
        // then only the real file is reported, with its true size
        assert_eq!(files.len(), 1, "{files:?}");
        assert_eq!(files[0]["name"], "app.log");
        assert_eq!(files[0]["size"], 5);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_tail_broadcasts_lines_to_live_subscribers() {
        // given a subscriber attached before any line arrives
        let tail = LogTail::default();
        let mut live = tail.subscribe();
        // when a line is pushed
        tail.push("first".to_string());
        // then the subscriber receives it and the snapshot still retains it
        assert_eq!(live.try_recv().expect("broadcast line"), "first");
        assert_eq!(tail.snapshot(), vec!["first".to_string()]);
    }

    #[test]
    fn pushing_without_subscribers_still_retains_the_line() {
        // given nobody is streaming
        let tail = LogTail::default();
        // when a line is pushed
        tail.push("orphan".to_string());
        // then the failed broadcast does not lose the retained tail
        assert_eq!(tail.snapshot(), vec!["orphan".to_string()]);
    }

    struct TempLogDir(std::path::PathBuf);

    impl TempLogDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "mahoquot-log-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::remove_dir_all(&dir).ok();
            Self(dir)
        }

        fn settings(&self, max_total_mb: i64, segment_mb: i64) -> Settings {
            Settings {
                auth_dir: self.0.to_string_lossy().to_string(),
                logging_to_file: true,
                logs_max_total_size_mb: max_total_mb,
                logs_segment_size_mb: segment_mb,
                ..Settings::default()
            }
        }

        fn logs(&self) -> std::path::PathBuf {
            self.0.join("logs")
        }

        fn total_bytes(&self) -> u64 {
            std::fs::read_dir(self.logs())
                .map(|entries| {
                    entries
                        .flatten()
                        .filter_map(|e| e.metadata().ok())
                        .filter(|m| m.is_file())
                        .map(|m| m.len())
                        .sum()
                })
                .unwrap_or(0)
        }

        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.logs())
                .map(|entries| {
                    entries
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names
        }
    }

    impl Drop for TempLogDir {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).ok();
        }
    }

    #[test]
    fn the_active_log_rotates_into_a_numbered_segment_at_the_segment_size() {
        // given a 1 MiB segment size and a generous total cap
        let temp = TempLogDir::new("rotate");
        let settings = temp.settings(64, 1);
        let line = "x".repeat(600_000);
        // when more than one segment worth of lines is written
        append_log_line(&settings, &line);
        append_log_line(&settings, &line);
        append_log_line(&settings, &line);
        // then the overflow moved into a segment and the active file restarted
        let names = temp.names();
        assert!(
            names.iter().any(|name| name.starts_with("gateway.log.")),
            "expected a rotated segment, found {names:?}"
        );
        let active = std::fs::metadata(temp.logs().join("gateway.log"))
            .expect("active log")
            .len();
        assert!(
            active <= 1024 * 1024,
            "active file must stay under the segment size, was {active}"
        );
    }

    #[test]
    fn rotation_drops_the_oldest_segments_to_hold_the_total_cap() {
        // given a 2 MiB total cap split into 1 MiB segments
        let temp = TempLogDir::new("cap");
        let settings = temp.settings(2, 1);
        let line = "y".repeat(600_000);
        // when far more than the cap is written
        for _ in 0..20 {
            append_log_line(&settings, &line);
        }
        // then the retained total never exceeds the cap
        let total = temp.total_bytes();
        assert!(
            total <= 2 * 1024 * 1024,
            "retained total {total} exceeded the 2 MiB cap"
        );
    }

    #[test]
    fn reading_logs_returns_segments_oldest_first_so_the_tail_is_newest() {
        // given rotation across more than nine segments, where a plain string
        // sort would place "gateway.log.10" before "gateway.log.9"
        let temp = TempLogDir::new("order");
        let settings = temp.settings(4096, 1);
        for index in 0..14 {
            append_log_line(
                &settings,
                &format!("line-{index:02} {}", "z".repeat(600_000)),
            );
        }
        // when the stored lines are read back
        let lines = read_log_lines(&temp.logs());
        let ordinals: Vec<usize> = lines
            .iter()
            .filter_map(|line| line.strip_prefix("line-"))
            .filter_map(|rest| rest.get(..2).and_then(|n| n.parse::<usize>().ok()))
            .collect();
        // then they come back in write order, so the newest line is last
        let mut sorted = ordinals.clone();
        sorted.sort_unstable();
        assert_eq!(ordinals, sorted, "segments must read oldest-first");
        assert_eq!(
            ordinals.last().copied(),
            Some(13),
            "the newest line must be the tail"
        );
    }
}

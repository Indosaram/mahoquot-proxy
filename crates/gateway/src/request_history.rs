//! Request-level SQLite history for the gateway.
//!
//! All SQLite work runs on one dedicated blocking worker thread. Callers never
//! execute database work on a Tokio executor. The schema stores only a stable
//! key identifier (normally produced by [`stable_key_identifier`]); there is no
//! column or API for retaining a raw inbound API key. Legacy aggregate buckets
//! are intentionally not importable as request events.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::{params, params_from_iter, types::Value, Connection, OptionalExtension};

const SCHEMA_VERSION: i64 = 2;
const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
const DEFAULT_MAX_SIZE_BYTES: u64 = 512 * 1024 * 1024;
const DEFAULT_PRUNE_CHUNK_SIZE: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeBucket {
    Minute,
    Hour,
    Day,
}

impl TimeBucket {
    pub fn parse(value: &str) -> Result<Self, HistoryError> {
        match value {
            "minute" => Ok(Self::Minute),
            "hour" => Ok(Self::Hour),
            "day" => Ok(Self::Day),
            other => Err(HistoryError::InvalidTimeBucket(other.to_string())),
        }
    }

    fn width_ms(self) -> i64 {
        match self {
            Self::Minute => 60_000,
            Self::Hour => 3_600_000,
            Self::Day => 86_400_000,
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HistoryError {
    #[error("invalid time bucket: {0}")]
    InvalidTimeBucket(String),
    #[error("invalid time range: start {start_ms} must be before end {end_ms}")]
    InvalidTimeRange { start_ms: i64, end_ms: i64 },
    #[error("invalid request history event: {0}")]
    InvalidEvent(String),
    #[error("invalid model price: {0}")]
    InvalidPrice(String),
    #[error("request history value is out of SQLite integer range: {0}")]
    ValueOutOfRange(&'static str),
    #[error("request history database error: {0}")]
    Database(String),
    #[error("request history worker is unavailable")]
    WorkerUnavailable,
}

impl From<rusqlite::Error> for HistoryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct HistoryConfig {
    pub busy_timeout: Duration,
    pub retention: Option<Duration>,
    pub max_size_bytes: Option<u64>,
    pub prune_chunk_size: usize,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            busy_timeout: DEFAULT_BUSY_TIMEOUT,
            retention: Some(DEFAULT_RETENTION),
            max_size_bytes: Some(DEFAULT_MAX_SIZE_BYTES),
            prune_chunk_size: DEFAULT_PRUNE_CHUNK_SIZE,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PrunePolicy {
    pub retention: Option<Duration>,
    pub max_size_bytes: Option<u64>,
    pub chunk_size: usize,
}

impl From<&HistoryConfig> for PrunePolicy {
    fn from(config: &HistoryConfig) -> Self {
        Self {
            retention: config.retention,
            max_size_bytes: config.max_size_bytes,
            chunk_size: config.prune_chunk_size,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoryTotals {
    pub requests: u64,
    pub successful_requests: u64,
    pub failed_requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub total_latency_ms: u64,
    pub average_latency_ms: f64,
    pub estimated_cost_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEvent {
    pub event_id: String,
    pub occurred_at_ms: i64,
    pub account_identifier: String,
    pub provider: String,
    pub model: String,
    pub key_identifier: Option<String>,
    pub status_code: u16,
    pub succeeded: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelPrice {
    pub model: String,
    pub version: String,
    pub input_per_million: f64,
    pub output_per_million: f64,
    pub cached_input_per_million: f64,
    pub effective_from_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutcomeFilter {
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupDimension {
    Account,
    Provider,
    Model,
    Key,
    Status,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HistoryQuery {
    pub start_ms: Option<i64>,
    pub end_ms: Option<i64>,
    pub accounts: Vec<String>,
    pub providers: Vec<String>,
    pub models: Vec<String>,
    pub key_identifiers: Vec<String>,
    pub status_codes: Vec<u16>,
    pub outcomes: Vec<OutcomeFilter>,
    pub search: Option<String>,
    pub time_bucket: Option<TimeBucket>,
    pub group_by: Vec<GroupDimension>,
}

impl HistoryQuery {
    fn validate(&self) -> Result<(), HistoryError> {
        if let (Some(start_ms), Some(end_ms)) = (self.start_ms, self.end_ms) {
            if start_ms >= end_ms {
                return Err(HistoryError::InvalidTimeRange { start_ms, end_ms });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupKey {
    pub bucket_start_ms: Option<i64>,
    pub account: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub key_identifier: Option<String>,
    pub status_code: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoryGroup {
    pub key: GroupKey,
    pub totals: HistoryTotals,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoryQueryResult {
    pub totals: HistoryTotals,
    pub groups: Vec<HistoryGroup>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HistoryEventRow {
    pub row_id: i64,
    pub event_id: String,
    pub occurred_at_ms: i64,
    pub account_identifier: String,
    pub provider: String,
    pub model: String,
    pub key_identifier: Option<String>,
    pub status_code: u16,
    pub succeeded: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
    pub latency_ms: u64,
    pub estimated_cost_usd: f64,
    pub price_version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HistoryEventPage {
    pub events: Vec<HistoryEventRow>,
    pub next_cursor: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PruneResult {
    pub deleted_events: u64,
    pub chunks: u64,
    pub logical_size_bytes: u64,
    pub size_cap_satisfied: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRecord {
    pub import_id: String,
    pub source: String,
    pub imported_at_ms: i64,
    pub event_count: u64,
}

#[derive(Debug, Clone)]
pub struct RequestHistory {
    worker: Arc<WorkerHandle>,
    config: HistoryConfig,
}

#[derive(Clone)]
pub struct HistoryService {
    sender: Option<mpsc::SyncSender<IngestCommand>>,
    store: Option<RequestHistory>,
    health: Arc<HistoryHealthState>,
    metrics: Arc<crate::metrics::GatewayMetrics>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryHealth {
    pub ready: bool,
    pub degraded: bool,
    pub queue_capacity: usize,
    pub queue_depth: u64,
    pub enqueued_events: u64,
    pub written_events: u64,
    pub dropped_events: u64,
    pub database_failures: u64,
    pub last_error: Option<String>,
}

#[derive(Debug)]
enum IngestCommand {
    Event(UsageEvent),
    Flush(mpsc::SyncSender<()>),
}

struct HistoryHealthState {
    ready: AtomicBool,
    degraded: AtomicBool,
    queue_capacity: usize,
    queue_depth: AtomicU64,
    enqueued_events: AtomicU64,
    written_events: AtomicU64,
    dropped_events: AtomicU64,
    database_failures: AtomicU64,
    last_error: Mutex<Option<String>>,
}

#[derive(Debug)]
pub enum HistoryState {
    Ready(RequestHistory),
    Degraded { error: String },
}

#[derive(Debug)]
struct WorkerHandle {
    sender: Mutex<Option<mpsc::Sender<Command>>>,
    join: Mutex<Option<JoinHandle<()>>>,
}

impl Drop for WorkerHandle {
    fn drop(&mut self) {
        if let Some(sender) = self
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = sender.send(Command::Shutdown);
        }
        if let Some(join) = self
            .join
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = join.join();
        }
    }
}

type Reply<T> = mpsc::SyncSender<Result<T, HistoryError>>;

enum Command {
    Insert(UsageEvent, Reply<bool>),
    InsertBatch(Vec<UsageEvent>, Reply<usize>),
    Query(HistoryQuery, Reply<HistoryQueryResult>),
    Count(HistoryQuery, Reply<u64>),
    Page(HistoryQuery, Option<i64>, usize, Reply<HistoryEventPage>),
    Detail(String, Reply<Option<HistoryEventRow>>),
    Export(HistoryQuery, Reply<Vec<HistoryEventRow>>),
    Clear(HistoryQuery, Reply<u64>),
    SetPrice(ModelPrice, Reply<()>),
    ListPrices(Reply<Vec<ModelPrice>>),
    DeletePrice(String, Option<String>, Reply<u64>),
    RecomputeCosts(Reply<u64>),
    Prune(i64, PrunePolicy, Reply<PruneResult>),
    SetMetadata(String, String, Reply<()>),
    GetMetadata(String, Reply<Option<String>>),
    RecordImport(ImportRecord, Reply<bool>),
    SchemaVersion(Reply<i64>),
    Explain(HistoryQuery, Reply<Vec<String>>),
    Shutdown,
}

impl HistoryService {
    pub fn open(
        path: &Path,
        queue_capacity: usize,
        batch_size: usize,
        metrics: Arc<crate::metrics::GatewayMetrics>,
    ) -> Self {
        let queue_capacity = queue_capacity.max(1);
        let health = Arc::new(HistoryHealthState {
            ready: AtomicBool::new(false),
            degraded: AtomicBool::new(false),
            queue_capacity,
            queue_depth: AtomicU64::new(0),
            enqueued_events: AtomicU64::new(0),
            written_events: AtomicU64::new(0),
            dropped_events: AtomicU64::new(0),
            database_failures: AtomicU64::new(0),
            last_error: Mutex::new(None),
        });
        let store = match RequestHistory::open(path) {
            HistoryState::Ready(store) => store,
            HistoryState::Degraded { error } => {
                health.degraded.store(true, Ordering::Relaxed);
                *health.last_error.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
                return Self {
                    sender: None,
                    store: None,
                    health,
                    metrics,
                };
            }
        };
        health.ready.store(true, Ordering::Relaxed);
        let (sender, receiver) = mpsc::sync_channel(queue_capacity);
        let worker_store = store.clone();
        let worker_health = Arc::clone(&health);
        let worker_metrics = Arc::clone(&metrics);
        std::thread::Builder::new()
            .name("mahoquot-history-ingest".to_string())
            .spawn(move || {
                let batch_size = batch_size.max(1);
                while let Ok(command) = receiver.recv() {
                    let mut batch = Vec::with_capacity(batch_size);
                    let mut flushes = Vec::new();
                    match command {
                        IngestCommand::Event(event) => batch.push(event),
                        IngestCommand::Flush(reply) => flushes.push(reply),
                    }
                    while batch.len() < batch_size {
                        match receiver.try_recv() {
                            Ok(IngestCommand::Event(event)) => batch.push(event),
                            Ok(IngestCommand::Flush(reply)) => {
                                flushes.push(reply);
                                break;
                            }
                            Err(mpsc::TryRecvError::Empty) => break,
                            Err(mpsc::TryRecvError::Disconnected) => break,
                        }
                    }
                    worker_health
                        .queue_depth
                        .fetch_sub(batch.len() as u64, Ordering::Relaxed);
                    let result = worker_store.insert_batch(&batch);
                    match result {
                        Ok(written) => {
                            worker_health
                                .written_events
                                .fetch_add(written as u64, Ordering::Relaxed);
                        }
                        Err(error) => {
                            worker_health.degraded.store(true, Ordering::Relaxed);
                            worker_health
                                .database_failures
                                .fetch_add(1, Ordering::Relaxed);
                            worker_metrics
                                .history_database_failures
                                .fetch_add(1, Ordering::Relaxed);
                            *worker_health
                                .last_error
                                .lock()
                                .unwrap_or_else(|p| p.into_inner()) = Some(error.to_string());
                        }
                    }
                    for reply in flushes {
                        let _ = reply.send(());
                    }
                }
            })
            .expect("start history ingestion worker");
        Self {
            sender: Some(sender),
            store: Some(store),
            health,
            metrics,
        }
    }

    pub fn enqueue(&self, event: UsageEvent) -> bool {
        let Some(sender) = &self.sender else {
            self.drop_event("request history is unavailable");
            return false;
        };
        match sender.try_send(IngestCommand::Event(event)) {
            Ok(()) => {
                self.health.enqueued_events.fetch_add(1, Ordering::Relaxed);
                self.health.queue_depth.fetch_add(1, Ordering::Relaxed);
                true
            }
            Err(error) => {
                self.drop_event(match error {
                    mpsc::TrySendError::Full(_) => "request history queue is full",
                    mpsc::TrySendError::Disconnected(_) => "request history queue is disconnected",
                });
                false
            }
        }
    }

    pub fn flush(&self) -> Result<(), HistoryError> {
        let sender = self
            .sender
            .as_ref()
            .ok_or(HistoryError::WorkerUnavailable)?;
        let (reply, receiver) = mpsc::sync_channel(1);
        sender
            .send(IngestCommand::Flush(reply))
            .map_err(|_| HistoryError::WorkerUnavailable)?;
        receiver.recv().map_err(|_| HistoryError::WorkerUnavailable)
    }

    fn drop_event(&self, message: &str) {
        self.health.degraded.store(true, Ordering::Relaxed);
        self.health.dropped_events.fetch_add(1, Ordering::Relaxed);
        self.metrics.history_dropped.fetch_add(1, Ordering::Relaxed);
        *self
            .health
            .last_error
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(message.to_string());
    }

    pub fn health(&self) -> HistoryHealth {
        HistoryHealth {
            ready: self.health.ready.load(Ordering::Relaxed),
            degraded: self.health.degraded.load(Ordering::Relaxed),
            queue_capacity: self.health.queue_capacity,
            queue_depth: self.health.queue_depth.load(Ordering::Relaxed),
            enqueued_events: self.health.enqueued_events.load(Ordering::Relaxed),
            written_events: self.health.written_events.load(Ordering::Relaxed),
            dropped_events: self.health.dropped_events.load(Ordering::Relaxed),
            database_failures: self.health.database_failures.load(Ordering::Relaxed),
            last_error: self
                .health
                .last_error
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone(),
        }
    }

    pub fn store(&self) -> Result<&RequestHistory, HistoryError> {
        self.store.as_ref().ok_or(HistoryError::WorkerUnavailable)
    }
}

impl RequestHistory {
    pub fn open(path: &Path) -> HistoryState {
        Self::open_with_config(path, HistoryConfig::default())
    }

    pub fn open_with_config(path: &Path, config: HistoryConfig) -> HistoryState {
        let path = path.to_path_buf();
        let (sender, receiver) = mpsc::channel();
        let (init_sender, init_receiver) = mpsc::sync_channel(1);
        let worker_path = path.clone();
        let worker_config = config.clone();
        let join = match std::thread::Builder::new()
            .name("mahoquot-request-history".to_string())
            .spawn(
                move || match open_connection(&worker_path, &worker_config) {
                    Ok(connection) => {
                        let _ = init_sender.send(Ok(()));
                        worker_loop(connection, &worker_path, receiver);
                    }
                    Err(error) => {
                        let _ = init_sender.send(Err(error));
                    }
                },
            ) {
            Ok(join) => join,
            Err(error) => {
                return HistoryState::Degraded {
                    error: format!("failed to start request history worker: {error}"),
                };
            }
        };

        match init_receiver.recv() {
            Ok(Ok(())) => HistoryState::Ready(Self {
                worker: Arc::new(WorkerHandle {
                    sender: Mutex::new(Some(sender)),
                    join: Mutex::new(Some(join)),
                }),
                config,
            }),
            Ok(Err(error)) => {
                let _ = join.join();
                HistoryState::Degraded {
                    error: error.to_string(),
                }
            }
            Err(_) => {
                let _ = join.join();
                HistoryState::Degraded {
                    error: format!(
                        "request history worker exited while opening {}",
                        path.display()
                    ),
                }
            }
        }
    }

    pub fn insert(&self, event: &UsageEvent) -> Result<bool, HistoryError> {
        validate_event(event)?;
        self.request(|reply| Command::Insert(event.clone(), reply))
    }

    pub fn insert_batch(&self, events: &[UsageEvent]) -> Result<usize, HistoryError> {
        for event in events {
            validate_event(event)?;
        }
        self.request(|reply| Command::InsertBatch(events.to_vec(), reply))
    }

    pub fn page(
        &self,
        query: &HistoryQuery,
        cursor: Option<i64>,
        limit: usize,
    ) -> Result<HistoryEventPage, HistoryError> {
        query.validate()?;
        if cursor.is_some_and(|value| value <= 0) {
            return Err(HistoryError::InvalidEvent(
                "cursor must be a positive row identifier".to_string(),
            ));
        }
        if limit == 0 || limit > 10_000 {
            return Err(HistoryError::InvalidEvent(
                "page limit must be between 1 and 10000".to_string(),
            ));
        }
        self.request(|reply| Command::Page(query.clone(), cursor, limit, reply))
    }

    pub fn count(&self, query: &HistoryQuery) -> Result<u64, HistoryError> {
        query.validate()?;
        self.request(|reply| Command::Count(query.clone(), reply))
    }

    pub fn detail(&self, event_id: &str) -> Result<Option<HistoryEventRow>, HistoryError> {
        if event_id.trim().is_empty() {
            return Err(HistoryError::InvalidEvent(
                "event id must not be empty".to_string(),
            ));
        }
        self.request(|reply| Command::Detail(event_id.to_string(), reply))
    }

    pub fn export(&self, query: &HistoryQuery) -> Result<Vec<HistoryEventRow>, HistoryError> {
        query.validate()?;
        self.request(|reply| Command::Export(query.clone(), reply))
    }

    pub fn clear(&self, query: &HistoryQuery) -> Result<u64, HistoryError> {
        query.validate()?;
        self.request(|reply| Command::Clear(query.clone(), reply))
    }

    pub fn totals(&self) -> Result<HistoryTotals, HistoryError> {
        Ok(self.query(&HistoryQuery::default())?.totals)
    }

    pub fn query(&self, query: &HistoryQuery) -> Result<HistoryQueryResult, HistoryError> {
        query.validate()?;
        self.request(|reply| Command::Query(query.clone(), reply))
    }

    pub fn set_model_price(&self, price: &ModelPrice) -> Result<(), HistoryError> {
        validate_price(price)?;
        self.request(|reply| Command::SetPrice(price.clone(), reply))
    }

    pub fn model_prices(&self) -> Result<Vec<ModelPrice>, HistoryError> {
        self.request(Command::ListPrices)
    }

    pub fn delete_model_price(
        &self,
        model: &str,
        version: Option<&str>,
    ) -> Result<u64, HistoryError> {
        self.request(|reply| {
            Command::DeletePrice(model.to_string(), version.map(ToString::to_string), reply)
        })
    }

    pub fn recompute_estimated_costs(&self) -> Result<u64, HistoryError> {
        self.request(Command::RecomputeCosts)
    }

    pub fn prune(&self, now_ms: i64) -> Result<PruneResult, HistoryError> {
        self.prune_with_policy(now_ms, PrunePolicy::from(&self.config))
    }

    pub fn prune_with_policy(
        &self,
        now_ms: i64,
        policy: PrunePolicy,
    ) -> Result<PruneResult, HistoryError> {
        if policy.chunk_size == 0 {
            return Err(HistoryError::InvalidEvent(
                "prune chunk size must be greater than zero".to_string(),
            ));
        }
        self.request(|reply| Command::Prune(now_ms, policy, reply))
    }

    pub fn set_metadata(&self, key: &str, value: &str) -> Result<(), HistoryError> {
        self.request(|reply| Command::SetMetadata(key.to_string(), value.to_string(), reply))
    }

    pub fn metadata(&self, key: &str) -> Result<Option<String>, HistoryError> {
        self.request(|reply| Command::GetMetadata(key.to_string(), reply))
    }

    pub fn record_import(&self, record: &ImportRecord) -> Result<bool, HistoryError> {
        self.request(|reply| Command::RecordImport(record.clone(), reply))
    }

    pub fn schema_version(&self) -> Result<i64, HistoryError> {
        self.request(Command::SchemaVersion)
    }

    pub fn explain_query_plan(&self, query: &HistoryQuery) -> Result<Vec<String>, HistoryError> {
        query.validate()?;
        self.request(|reply| Command::Explain(query.clone(), reply))
    }

    fn request<T>(&self, build: impl FnOnce(Reply<T>) -> Command) -> Result<T, HistoryError> {
        let (reply_sender, reply_receiver) = mpsc::sync_channel(1);
        let sender = self
            .worker
            .sender
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .cloned()
            .ok_or(HistoryError::WorkerUnavailable)?;
        sender
            .send(build(reply_sender))
            .map_err(|_| HistoryError::WorkerUnavailable)?;
        reply_receiver
            .recv()
            .map_err(|_| HistoryError::WorkerUnavailable)?
    }
}

fn validate_event(event: &UsageEvent) -> Result<(), HistoryError> {
    if event.event_id.trim().is_empty() {
        return Err(HistoryError::InvalidEvent(
            "event_id must not be empty".to_string(),
        ));
    }
    if event.account_identifier.trim().is_empty() {
        return Err(HistoryError::InvalidEvent(
            "account_identifier must not be empty".to_string(),
        ));
    }
    if event.provider.trim().is_empty() || event.model.trim().is_empty() {
        return Err(HistoryError::InvalidEvent(
            "provider and model must not be empty".to_string(),
        ));
    }
    for (name, value) in [
        ("input_tokens", event.input_tokens),
        ("output_tokens", event.output_tokens),
        ("cached_input_tokens", event.cached_input_tokens),
        ("reasoning_tokens", event.reasoning_tokens),
        ("total_tokens", event.total_tokens),
        ("latency_ms", event.latency_ms),
    ] {
        i64::try_from(value).map_err(|_| HistoryError::ValueOutOfRange(name))?;
    }
    Ok(())
}

fn validate_price(price: &ModelPrice) -> Result<(), HistoryError> {
    if price.model.trim().is_empty() || price.version.trim().is_empty() {
        return Err(HistoryError::InvalidPrice(
            "model and version must not be empty".to_string(),
        ));
    }
    for value in [
        price.input_per_million,
        price.output_per_million,
        price.cached_input_per_million,
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(HistoryError::InvalidPrice(
                "rates must be finite and non-negative".to_string(),
            ));
        }
    }
    Ok(())
}

fn open_connection(path: &Path, config: &HistoryConfig) -> Result<Connection, HistoryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| HistoryError::Database(error.to_string()))?;
    }
    let mut connection = Connection::open(path)?;
    connection.busy_timeout(config.busy_timeout)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "NORMAL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;
    migrate(&mut connection)?;
    let check: String = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    if check != "ok" {
        return Err(HistoryError::Database(format!(
            "SQLite quick_check failed: {check}"
        )));
    }
    Ok(connection)
}

fn migrate(connection: &mut Connection) -> Result<(), HistoryError> {
    let current: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if current > SCHEMA_VERSION {
        return Err(HistoryError::Database(format!(
            "database schema version {current} is newer than supported version {SCHEMA_VERSION}"
        )));
    }
    if current < 1 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(MIGRATION_V1)?;
        transaction.execute(
            "INSERT OR IGNORE INTO schema_migrations(version, applied_at_ms) VALUES (1, ?1)",
            [unix_time_ms()],
        )?;
        transaction.pragma_update(None, "user_version", 1)?;
        transaction.commit()?;
    }
    if current < 2 {
        let transaction = connection.transaction()?;
        transaction.execute_batch(MIGRATION_V2)?;
        transaction.execute(
            "INSERT OR IGNORE INTO schema_migrations(version, applied_at_ms) VALUES (2, ?1)",
            [unix_time_ms()],
        )?;
        transaction.pragma_update(None, "user_version", 2)?;
        transaction.commit()?;
    }
    Ok(())
}

const MIGRATION_V1: &str = r#"
CREATE TABLE usage_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE,
    occurred_at_ms INTEGER NOT NULL,
    account_identifier TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT NOT NULL,
    key_identifier TEXT,
    status_code INTEGER NOT NULL,
    succeeded INTEGER NOT NULL CHECK (succeeded IN (0, 1)),
    input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    cached_input_tokens INTEGER NOT NULL DEFAULT 0 CHECK (cached_input_tokens >= 0),
    reasoning_tokens INTEGER NOT NULL DEFAULT 0 CHECK (reasoning_tokens >= 0),
    total_tokens INTEGER NOT NULL DEFAULT 0 CHECK (total_tokens >= 0),
    latency_ms INTEGER NOT NULL DEFAULT 0 CHECK (latency_ms >= 0),
    created_at_ms INTEGER NOT NULL
);
CREATE TABLE model_prices (
    model TEXT PRIMARY KEY,
    input_per_million REAL NOT NULL,
    output_per_million REAL NOT NULL,
    cached_input_per_million REAL NOT NULL DEFAULT 0,
    updated_at_ms INTEGER NOT NULL
);
CREATE TABLE history_metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE TABLE history_imports (
    import_id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    imported_at_ms INTEGER NOT NULL,
    event_count INTEGER NOT NULL CHECK (event_count >= 0)
);
CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY,
    applied_at_ms INTEGER NOT NULL
);
CREATE INDEX idx_usage_events_time ON usage_events(occurred_at_ms);
"#;

const MIGRATION_V2: &str = r#"
ALTER TABLE usage_events ADD COLUMN estimated_cost_usd REAL NOT NULL DEFAULT 0;
ALTER TABLE usage_events ADD COLUMN price_version TEXT;
ALTER TABLE model_prices RENAME TO model_prices_v1;
CREATE TABLE model_prices (
    model TEXT NOT NULL,
    version TEXT NOT NULL,
    input_per_million REAL NOT NULL CHECK (input_per_million >= 0),
    output_per_million REAL NOT NULL CHECK (output_per_million >= 0),
    cached_input_per_million REAL NOT NULL DEFAULT 0 CHECK (cached_input_per_million >= 0),
    effective_from_ms INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (model, version)
);
INSERT INTO model_prices(
    model, version, input_per_million, output_per_million,
    cached_input_per_million, effective_from_ms, created_at_ms
)
SELECT model, 'legacy', input_per_million, output_per_million,
       cached_input_per_million, 0, updated_at_ms
FROM model_prices_v1;
DROP TABLE model_prices_v1;
CREATE INDEX idx_model_prices_effective ON model_prices(model, effective_from_ms DESC);
CREATE INDEX idx_usage_events_account_time ON usage_events(account_identifier, occurred_at_ms);
CREATE INDEX idx_usage_events_provider_time ON usage_events(provider, occurred_at_ms);
CREATE INDEX idx_usage_events_model_time ON usage_events(model, occurred_at_ms);
CREATE INDEX idx_usage_events_key_time ON usage_events(key_identifier, occurred_at_ms);
CREATE INDEX idx_usage_events_status_time ON usage_events(status_code, occurred_at_ms);
"#;

fn worker_loop(mut connection: Connection, path: &Path, receiver: mpsc::Receiver<Command>) {
    while let Ok(command) = receiver.recv() {
        match command {
            Command::Insert(event, reply) => {
                let _ = reply.send(insert_event(&connection, &event));
            }
            Command::InsertBatch(events, reply) => {
                let _ = reply.send(insert_events(&mut connection, &events));
            }
            Command::Query(query, reply) => {
                let _ = reply.send(query_history(&connection, &query));
            }
            Command::Count(query, reply) => {
                let _ = reply.send(count_events(&connection, &query));
            }
            Command::Page(query, cursor, limit, reply) => {
                let _ = reply.send(page_events(&connection, &query, cursor, limit));
            }
            Command::Detail(event_id, reply) => {
                let _ = reply.send(detail_event(&connection, &event_id));
            }
            Command::Export(query, reply) => {
                let _ = reply.send(export_events(&connection, &query));
            }
            Command::Clear(query, reply) => {
                let _ = reply.send(clear_events(&mut connection, &query));
            }
            Command::SetPrice(price, reply) => {
                let _ = reply.send(set_price(&connection, &price));
            }
            Command::ListPrices(reply) => {
                let _ = reply.send(list_prices(&connection));
            }
            Command::DeletePrice(model, version, reply) => {
                let _ = reply.send(delete_price(&connection, &model, version.as_deref()));
            }
            Command::RecomputeCosts(reply) => {
                let _ = reply.send(recompute_costs(&mut connection));
            }
            Command::Prune(now_ms, policy, reply) => {
                let _ = reply.send(prune_events(&mut connection, path, now_ms, policy));
            }
            Command::SetMetadata(key, value, reply) => {
                let result = connection
                    .execute(
                        "INSERT INTO history_metadata(key, value, updated_at_ms) VALUES (?1, ?2, ?3) \
                         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at_ms=excluded.updated_at_ms",
                        params![key, value, unix_time_ms()],
                    )
                    .map(|_| ())
                    .map_err(HistoryError::from);
                let _ = reply.send(result);
            }
            Command::GetMetadata(key, reply) => {
                let result = connection
                    .query_row(
                        "SELECT value FROM history_metadata WHERE key=?1",
                        [key],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(HistoryError::from);
                let _ = reply.send(result);
            }
            Command::RecordImport(record, reply) => {
                let result = i64::try_from(record.event_count)
                    .map_err(|_| HistoryError::ValueOutOfRange("event_count"))
                    .and_then(|event_count| {
                        connection
                            .execute(
                                "INSERT OR IGNORE INTO history_imports(import_id, source, imported_at_ms, event_count) \
                                 VALUES (?1, ?2, ?3, ?4)",
                                params![record.import_id, record.source, record.imported_at_ms, event_count],
                            )
                            .map(|changed| changed == 1)
                            .map_err(HistoryError::from)
                    });
                let _ = reply.send(result);
            }
            Command::SchemaVersion(reply) => {
                let result = connection
                    .query_row("PRAGMA user_version", [], |row| row.get(0))
                    .map_err(HistoryError::from);
                let _ = reply.send(result);
            }
            Command::Explain(query, reply) => {
                let _ = reply.send(explain_query(&connection, &query));
            }
            Command::Shutdown => break,
        }
    }
}

fn insert_events(
    connection: &mut Connection,
    events: &[UsageEvent],
) -> Result<usize, HistoryError> {
    let transaction = connection.transaction()?;
    let mut inserted = 0;
    for event in events {
        inserted += usize::from(insert_event(&transaction, event)?);
    }
    transaction.commit()?;
    Ok(inserted)
}

fn insert_event(connection: &Connection, event: &UsageEvent) -> Result<bool, HistoryError> {
    let (estimated_cost_usd, price_version) = price_snapshot(connection, event)?;
    let changed = connection.execute(
        "INSERT OR IGNORE INTO usage_events(
            event_id, occurred_at_ms, account_identifier, provider, model, key_identifier,
            status_code, succeeded, input_tokens, output_tokens, cached_input_tokens,
            reasoning_tokens, total_tokens, latency_ms, created_at_ms,
            estimated_cost_usd, price_version
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17
         )",
        params![
            event.event_id,
            event.occurred_at_ms,
            event.account_identifier,
            event.provider,
            event.model,
            event.key_identifier,
            i64::from(event.status_code),
            i64::from(event.succeeded),
            to_sql_i64(event.input_tokens, "input_tokens")?,
            to_sql_i64(event.output_tokens, "output_tokens")?,
            to_sql_i64(event.cached_input_tokens, "cached_input_tokens")?,
            to_sql_i64(event.reasoning_tokens, "reasoning_tokens")?,
            to_sql_i64(event.total_tokens, "total_tokens")?,
            to_sql_i64(event.latency_ms, "latency_ms")?,
            unix_time_ms(),
            estimated_cost_usd,
            price_version,
        ],
    )?;
    Ok(changed == 1)
}

fn to_sql_i64(value: u64, name: &'static str) -> Result<i64, HistoryError> {
    i64::try_from(value).map_err(|_| HistoryError::ValueOutOfRange(name))
}

fn count_events(connection: &Connection, query: &HistoryQuery) -> Result<u64, HistoryError> {
    let (where_sql, values) = build_where(query);
    let sql = format!("SELECT COUNT(*) FROM usage_events e{where_sql}");
    Ok(
        connection.query_row(&sql, params_from_iter(values.iter()), |row| {
            row.get::<_, i64>(0)
        })? as u64,
    )
}

const EVENT_SELECT: &str =
    "SELECT e.id, e.event_id, e.occurred_at_ms, e.account_identifier, e.provider, e.model, \
         e.key_identifier, e.status_code, e.succeeded, e.input_tokens, e.output_tokens, \
         e.cached_input_tokens, e.reasoning_tokens, e.total_tokens, e.latency_ms, \
         e.estimated_cost_usd, e.price_version FROM usage_events e";

fn read_event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryEventRow> {
    Ok(HistoryEventRow {
        row_id: row.get(0)?,
        event_id: row.get(1)?,
        occurred_at_ms: row.get(2)?,
        account_identifier: row.get(3)?,
        provider: row.get(4)?,
        model: row.get(5)?,
        key_identifier: row.get(6)?,
        status_code: row.get::<_, i64>(7)? as u16,
        succeeded: row.get::<_, i64>(8)? != 0,
        input_tokens: row.get::<_, i64>(9)? as u64,
        output_tokens: row.get::<_, i64>(10)? as u64,
        cached_input_tokens: row.get::<_, i64>(11)? as u64,
        reasoning_tokens: row.get::<_, i64>(12)? as u64,
        total_tokens: row.get::<_, i64>(13)? as u64,
        latency_ms: row.get::<_, i64>(14)? as u64,
        estimated_cost_usd: row.get(15)?,
        price_version: row.get(16)?,
    })
}

fn detail_event(
    connection: &Connection,
    event_id: &str,
) -> Result<Option<HistoryEventRow>, HistoryError> {
    Ok(connection
        .query_row(
            &format!("{EVENT_SELECT} WHERE e.event_id=?1"),
            [event_id],
            read_event_row,
        )
        .optional()?)
}

fn export_events(
    connection: &Connection,
    query: &HistoryQuery,
) -> Result<Vec<HistoryEventRow>, HistoryError> {
    let (where_sql, values) = build_where(query);
    let sql = format!("{EVENT_SELECT}{where_sql} ORDER BY e.id DESC");
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(values.iter()), read_event_row)?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn clear_events(connection: &mut Connection, query: &HistoryQuery) -> Result<u64, HistoryError> {
    let (where_sql, values) = build_where(query);
    let sql = format!(
        "DELETE FROM usage_events WHERE id IN (SELECT e.id FROM usage_events e{where_sql})"
    );
    let transaction = connection.transaction()?;
    let deleted = transaction.execute(&sql, params_from_iter(values.iter()))? as u64;
    transaction.commit()?;
    Ok(deleted)
}

fn page_events(
    connection: &Connection,
    query: &HistoryQuery,
    cursor: Option<i64>,
    limit: usize,
) -> Result<HistoryEventPage, HistoryError> {
    let (where_sql, mut values) = build_where(query);
    let cursor_clause = if cursor.is_some() {
        if where_sql.is_empty() {
            " WHERE e.id < ?"
        } else {
            " AND e.id < ?"
        }
    } else {
        ""
    };
    if let Some(cursor) = cursor {
        values.push(Value::Integer(cursor));
    }
    values.push(Value::Integer((limit + 1) as i64));
    let sql = format!("{EVENT_SELECT}{where_sql}{cursor_clause} ORDER BY e.id DESC LIMIT ?");
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(values.iter()), read_event_row)?;
    let mut events = rows.collect::<Result<Vec<_>, _>>()?;
    let next_cursor = if events.len() > limit {
        events.pop();
        events.last().map(|event| event.row_id)
    } else {
        None
    };
    Ok(HistoryEventPage {
        events,
        next_cursor,
    })
}

fn set_price(connection: &Connection, price: &ModelPrice) -> Result<(), HistoryError> {
    connection.execute(
        "INSERT INTO model_prices(
            model, version, input_per_million, output_per_million,
            cached_input_per_million, effective_from_ms, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(model, version) DO UPDATE SET
            input_per_million=excluded.input_per_million,
            output_per_million=excluded.output_per_million,
            cached_input_per_million=excluded.cached_input_per_million,
            effective_from_ms=excluded.effective_from_ms",
        params![
            price.model,
            price.version,
            price.input_per_million,
            price.output_per_million,
            price.cached_input_per_million,
            price.effective_from_ms,
            unix_time_ms(),
        ],
    )?;
    Ok(())
}

fn list_prices(connection: &Connection) -> Result<Vec<ModelPrice>, HistoryError> {
    let mut statement = connection.prepare(
        "SELECT model, version, input_per_million, output_per_million, \
         cached_input_per_million, effective_from_ms \
         FROM model_prices ORDER BY model, effective_from_ms DESC, version",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(ModelPrice {
            model: row.get(0)?,
            version: row.get(1)?,
            input_per_million: row.get(2)?,
            output_per_million: row.get(3)?,
            cached_input_per_million: row.get(4)?,
            effective_from_ms: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn delete_price(
    connection: &Connection,
    model: &str,
    version: Option<&str>,
) -> Result<u64, HistoryError> {
    let changed = match version {
        Some(version) => connection.execute(
            "DELETE FROM model_prices WHERE model=?1 AND version=?2",
            params![model, version],
        )?,
        None => connection.execute("DELETE FROM model_prices WHERE model=?1", [model])?,
    };
    Ok(changed as u64)
}

fn price_snapshot(
    connection: &Connection,
    event: &UsageEvent,
) -> Result<(f64, Option<String>), HistoryError> {
    let price = connection
        .query_row(
            "SELECT version, input_per_million, output_per_million, cached_input_per_million
             FROM model_prices
             WHERE model=?1 AND effective_from_ms <= ?2
             ORDER BY effective_from_ms DESC, created_at_ms DESC
             LIMIT 1",
            params![event.model, event.occurred_at_ms],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, f64>(3)?,
                ))
            },
        )
        .optional()?;
    Ok(match price {
        Some((version, input_rate, output_rate, cached_rate)) => (
            estimated_cost(
                event.input_tokens,
                event.output_tokens,
                event.cached_input_tokens,
                input_rate,
                output_rate,
                cached_rate,
            ),
            Some(version),
        ),
        None => (0.0, None),
    })
}

fn estimated_cost(
    input_tokens: u64,
    output_tokens: u64,
    cached_input_tokens: u64,
    input_rate: f64,
    output_rate: f64,
    cached_rate: f64,
) -> f64 {
    let cached = cached_input_tokens.min(input_tokens);
    let uncached = input_tokens - cached;
    (uncached as f64 * input_rate
        + output_tokens as f64 * output_rate
        + cached as f64 * cached_rate)
        / 1_000_000.0
}

fn recompute_costs(connection: &mut Connection) -> Result<u64, HistoryError> {
    let transaction = connection.transaction()?;
    let events = {
        let mut statement = transaction.prepare(
            "SELECT id, model, occurred_at_ms, input_tokens, output_tokens, cached_input_tokens
             FROM usage_events ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)? as u64,
                row.get::<_, i64>(4)? as u64,
                row.get::<_, i64>(5)? as u64,
            ))
        })?;
        rows.collect::<Result<Vec<_>, _>>()?
    };
    let mut updated = 0_u64;
    for (id, model, occurred_at_ms, input, output, cached) in events {
        let price = transaction
            .query_row(
                "SELECT version, input_per_million, output_per_million, cached_input_per_million
                 FROM model_prices
                 WHERE model=?1 AND effective_from_ms <= ?2
                 ORDER BY effective_from_ms DESC, created_at_ms DESC
                 LIMIT 1",
                params![model, occurred_at_ms],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, f64>(1)?,
                        row.get::<_, f64>(2)?,
                        row.get::<_, f64>(3)?,
                    ))
                },
            )
            .optional()?;
        let (cost, version) = match price {
            Some((version, input_rate, output_rate, cached_rate)) => (
                estimated_cost(input, output, cached, input_rate, output_rate, cached_rate),
                Some(version),
            ),
            None => (0.0, None),
        };
        transaction.execute(
            "UPDATE usage_events SET estimated_cost_usd=?1, price_version=?2 WHERE id=?3",
            params![cost, version, id],
        )?;
        updated += 1;
    }
    transaction.commit()?;
    Ok(updated)
}

fn query_history(
    connection: &Connection,
    query: &HistoryQuery,
) -> Result<HistoryQueryResult, HistoryError> {
    let (where_sql, query_params) = build_where(query);
    let totals_sql = format!(
        "SELECT COUNT(*),
            COALESCE(SUM(succeeded), 0),
            COALESCE(SUM(CASE WHEN succeeded=0 THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(SUM(cached_input_tokens), 0),
            COALESCE(SUM(reasoning_tokens), 0),
            COALESCE(SUM(total_tokens), 0),
            COALESCE(SUM(latency_ms), 0),
            COALESCE(AVG(latency_ms), 0.0),
            COALESCE(SUM(estimated_cost_usd), 0.0)
         FROM usage_events e{where_sql}"
    );
    let totals = connection.query_row(
        &totals_sql,
        params_from_iter(query_params.iter()),
        read_totals,
    )?;

    let groups = if query.time_bucket.is_none() && query.group_by.is_empty() {
        Vec::new()
    } else {
        query_groups(connection, query, &where_sql, &query_params)?
    };
    Ok(HistoryQueryResult { totals, groups })
}

fn read_totals(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryTotals> {
    Ok(HistoryTotals {
        requests: row.get::<_, i64>(0)? as u64,
        successful_requests: row.get::<_, i64>(1)? as u64,
        failed_requests: row.get::<_, i64>(2)? as u64,
        input_tokens: row.get::<_, i64>(3)? as u64,
        output_tokens: row.get::<_, i64>(4)? as u64,
        cached_input_tokens: row.get::<_, i64>(5)? as u64,
        reasoning_tokens: row.get::<_, i64>(6)? as u64,
        total_tokens: row.get::<_, i64>(7)? as u64,
        total_latency_ms: row.get::<_, i64>(8)? as u64,
        average_latency_ms: row.get(9)?,
        estimated_cost_usd: row.get(10)?,
    })
}

fn query_groups(
    connection: &Connection,
    query: &HistoryQuery,
    where_sql: &str,
    query_params: &[Value],
) -> Result<Vec<HistoryGroup>, HistoryError> {
    let mut selectors = Vec::<(&'static str, &'static str)>::new();
    if query.time_bucket.is_some() {
        selectors.push(("bucket", "bucket"));
    }
    for dimension in &query.group_by {
        let selector = match dimension {
            GroupDimension::Account => ("account_identifier", "account"),
            GroupDimension::Provider => ("provider", "provider"),
            GroupDimension::Model => ("model", "model"),
            GroupDimension::Key => ("key_identifier", "key_identifier"),
            GroupDimension::Status => ("status_code", "status_code"),
        };
        if !selectors.iter().any(|(_, alias)| *alias == selector.1) {
            selectors.push(selector);
        }
    }

    let bucket_expression = query.time_bucket.map(|bucket| {
        let width = bucket.width_ms();
        format!("(occurred_at_ms / {width}) * {width}")
    });
    let select_sql = selectors
        .iter()
        .map(|(column, alias)| {
            if *alias == "bucket" {
                format!("{} AS bucket", bucket_expression.as_deref().unwrap_or("0"))
            } else {
                format!("e.{column} AS {alias}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let group_sql = (1..=selectors.len())
        .map(|index| index.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {select_sql},
            COUNT(*),
            COALESCE(SUM(succeeded), 0),
            COALESCE(SUM(CASE WHEN succeeded=0 THEN 1 ELSE 0 END), 0),
            COALESCE(SUM(input_tokens), 0),
            COALESCE(SUM(output_tokens), 0),
            COALESCE(SUM(cached_input_tokens), 0),
            COALESCE(SUM(reasoning_tokens), 0),
            COALESCE(SUM(total_tokens), 0),
            COALESCE(SUM(latency_ms), 0),
            COALESCE(AVG(latency_ms), 0.0),
            COALESCE(SUM(estimated_cost_usd), 0.0)
         FROM usage_events e{where_sql}
         GROUP BY {group_sql}
         ORDER BY {group_sql}"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(query_params.iter()), |row| {
        let mut key = GroupKey::default();
        for (index, (_, alias)) in selectors.iter().enumerate() {
            match *alias {
                "bucket" => key.bucket_start_ms = row.get(index)?,
                "account" => key.account = row.get(index)?,
                "provider" => key.provider = row.get(index)?,
                "model" => key.model = row.get(index)?,
                "key_identifier" => key.key_identifier = row.get(index)?,
                "status_code" => {
                    key.status_code = row.get::<_, Option<i64>>(index)?.map(|value| value as u16)
                }
                _ => unreachable!("known group alias"),
            }
        }
        let totals_start = selectors.len();
        Ok(HistoryGroup {
            key,
            totals: HistoryTotals {
                requests: row.get::<_, i64>(totals_start)? as u64,
                successful_requests: row.get::<_, i64>(totals_start + 1)? as u64,
                failed_requests: row.get::<_, i64>(totals_start + 2)? as u64,
                input_tokens: row.get::<_, i64>(totals_start + 3)? as u64,
                output_tokens: row.get::<_, i64>(totals_start + 4)? as u64,
                cached_input_tokens: row.get::<_, i64>(totals_start + 5)? as u64,
                reasoning_tokens: row.get::<_, i64>(totals_start + 6)? as u64,
                total_tokens: row.get::<_, i64>(totals_start + 7)? as u64,
                total_latency_ms: row.get::<_, i64>(totals_start + 8)? as u64,
                average_latency_ms: row.get(totals_start + 9)?,
                estimated_cost_usd: row.get(totals_start + 10)?,
            },
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn build_where(query: &HistoryQuery) -> (String, Vec<Value>) {
    let mut clauses = Vec::new();
    let mut values = Vec::new();
    if let Some(start_ms) = query.start_ms {
        clauses.push("e.occurred_at_ms >= ?".to_string());
        values.push(Value::Integer(start_ms));
    }
    if let Some(end_ms) = query.end_ms {
        clauses.push("e.occurred_at_ms < ?".to_string());
        values.push(Value::Integer(end_ms));
    }
    push_text_filter(
        &mut clauses,
        &mut values,
        "e.account_identifier",
        &query.accounts,
    );
    push_text_filter(&mut clauses, &mut values, "e.provider", &query.providers);
    push_text_filter(&mut clauses, &mut values, "e.model", &query.models);
    push_text_filter(
        &mut clauses,
        &mut values,
        "e.key_identifier",
        &query.key_identifiers,
    );
    if !query.status_codes.is_empty() {
        clauses.push(format!(
            "e.status_code IN ({})",
            placeholders(query.status_codes.len())
        ));
        values.extend(
            query
                .status_codes
                .iter()
                .map(|status| Value::Integer(i64::from(*status))),
        );
    }
    let wants_success = query.outcomes.contains(&OutcomeFilter::Succeeded);
    let wants_failure = query.outcomes.contains(&OutcomeFilter::Failed);
    match (wants_success, wants_failure) {
        (true, false) => clauses.push("e.succeeded = 1".to_string()),
        (false, true) => clauses.push("e.succeeded = 0".to_string()),
        _ => {}
    }
    if let Some(search) = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let pattern = format!("%{}%", escape_like(search));
        clauses.push(
            "(e.event_id LIKE ? ESCAPE '\\' OR e.account_identifier LIKE ? ESCAPE '\\' OR e.provider LIKE ? ESCAPE '\\' OR e.model LIKE ? ESCAPE '\\' OR COALESCE(e.key_identifier, '') LIKE ? ESCAPE '\\')"
                .to_string(),
        );
        values.extend(std::iter::repeat_n(Value::Text(pattern), 5));
    }
    let where_sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    (where_sql, values)
}

fn push_text_filter(
    clauses: &mut Vec<String>,
    values: &mut Vec<Value>,
    column: &str,
    selected: &[String],
) {
    if selected.is_empty() {
        return;
    }
    clauses.push(format!("{column} IN ({})", placeholders(selected.len())));
    values.extend(selected.iter().cloned().map(Value::Text));
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(", ")
}

fn explain_query(
    connection: &Connection,
    query: &HistoryQuery,
) -> Result<Vec<String>, HistoryError> {
    let (where_sql, query_params) = build_where(query);
    let sql = format!(
        "EXPLAIN QUERY PLAN SELECT event_id FROM usage_events e{where_sql} ORDER BY occurred_at_ms"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(query_params.iter()), |row| row.get(3))?;
    Ok(rows.collect::<Result<Vec<String>, _>>()?)
}

fn prune_events(
    connection: &mut Connection,
    _path: &Path,
    now_ms: i64,
    policy: PrunePolicy,
) -> Result<PruneResult, HistoryError> {
    let chunk_size = i64::try_from(policy.chunk_size)
        .map_err(|_| HistoryError::ValueOutOfRange("prune_chunk_size"))?;
    let mut result = PruneResult::default();
    if let Some(retention) = policy.retention {
        let retention_ms = i64::try_from(retention.as_millis()).unwrap_or(i64::MAX);
        let cutoff = now_ms.saturating_sub(retention_ms);
        loop {
            let deleted = connection.execute(
                "DELETE FROM usage_events WHERE id IN (
                    SELECT id FROM usage_events
                    WHERE occurred_at_ms < ?1
                    ORDER BY occurred_at_ms, id
                    LIMIT ?2
                 )",
                params![cutoff, chunk_size],
            )?;
            if deleted == 0 {
                break;
            }
            result.deleted_events += deleted as u64;
            result.chunks += 1;
        }
    }

    if let Some(max_size_bytes) = policy.max_size_bytes {
        while logical_database_size(connection)? > max_size_bytes {
            let deleted = connection.execute(
                "DELETE FROM usage_events WHERE id IN (
                    SELECT id FROM usage_events ORDER BY occurred_at_ms, id LIMIT ?1
                 )",
                [chunk_size],
            )?;
            if deleted == 0 {
                break;
            }
            result.deleted_events += deleted as u64;
            result.chunks += 1;
        }
    }
    let _ = connection.execute_batch("PRAGMA wal_checkpoint(PASSIVE);");
    result.logical_size_bytes = logical_database_size(connection)?;
    result.size_cap_satisfied = policy
        .max_size_bytes
        .map(|cap| result.logical_size_bytes <= cap)
        .unwrap_or(true);
    Ok(result)
}

fn logical_database_size(connection: &Connection) -> Result<u64, HistoryError> {
    let page_count: i64 = connection.query_row("PRAGMA page_count", [], |row| row.get(0))?;
    let free_pages: i64 = connection.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
    let page_size: i64 = connection.query_row("PRAGMA page_size", [], |row| row.get(0))?;
    Ok((page_count.saturating_sub(free_pages).max(0) as u64).saturating_mul(page_size as u64))
}

fn unix_time_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// Return a stable SHA-256 identifier for an inbound API key. Only this digest
/// (or a caller-provided non-secret label) belongs in [`UsageEvent::key_identifier`].
pub fn stable_key_identifier(raw_api_key: &str) -> String {
    let digest = sha256(raw_api_key.as_bytes());
    let mut rendered = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(rendered, "{byte:02x}");
    }
    rendered
}

// Small self-contained SHA-256 implementation keeps key material one-way
// without adding another cryptography dependency to the gateway.
fn sha256(input: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut message = input.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = [
        0x6a09e667_u32,
        0xbb67ae85,
        0x3c6ef372,
        0xa54ff53a,
        0x510e527f,
        0x9b05688c,
        0x1f83d9ab,
        0x5be0cd19,
    ];
    for chunk in message.chunks_exact(64) {
        let mut words = [0_u32; 64];
        for (index, bytes) in chunk.chunks_exact(4).enumerate() {
            words[index] = u32::from_be_bytes(bytes.try_into().expect("four-byte chunk"));
        }
        for index in 16..64 {
            let s0 = words[index - 15].rotate_right(7)
                ^ words[index - 15].rotate_right(18)
                ^ (words[index - 15] >> 3);
            let s1 = words[index - 2].rotate_right(17)
                ^ words[index - 2].rotate_right(19)
                ^ (words[index - 2] >> 10);
            words[index] = words[index - 16]
                .wrapping_add(s0)
                .wrapping_add(words[index - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sigma1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choice = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sigma1)
                .wrapping_add(choice)
                .wrapping_add(K[index])
                .wrapping_add(words[index]);
            let sigma0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sigma0.wrapping_add(majority);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h].into_iter()) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut digest = [0_u8; 32];
    for (chunk, value) in digest.chunks_exact_mut(4).zip(state) {
        chunk.copy_from_slice(&value.to_be_bytes());
    }
    digest
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_PATH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    pub(super) struct TestPath(pub(super) PathBuf);

    impl TestPath {
        pub(super) fn new(label: &str) -> Self {
            let sequence = TEST_PATH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            Self(std::env::temp_dir().join(format!(
                "mahoquot-request-history-{label}-{}-{sequence}.sqlite",
                std::process::id()
            )))
        }
    }

    impl Drop for TestPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            let _ = std::fs::remove_file(self.0.with_extension("sqlite-wal"));
            let _ = std::fs::remove_file(self.0.with_extension("sqlite-shm"));
        }
    }

    pub(super) fn ready(path: &Path) -> RequestHistory {
        ready_with_config(
            path,
            HistoryConfig {
                retention: None,
                max_size_bytes: None,
                ..HistoryConfig::default()
            },
        )
    }

    pub(super) fn ready_with_config(path: &Path, config: HistoryConfig) -> RequestHistory {
        match RequestHistory::open_with_config(path, config) {
            HistoryState::Ready(history) => history,
            HistoryState::Degraded { error } => panic!("history unexpectedly degraded: {error}"),
        }
    }

    pub(super) struct EventFixture<'a> {
        pub event_id: &'a str,
        pub occurred_at_ms: i64,
        pub account: &'a str,
        pub provider: &'a str,
        pub model: &'a str,
        pub key: &'a str,
        pub status_code: u16,
        pub input_tokens: u64,
        pub output_tokens: u64,
        pub cached_input_tokens: u64,
    }

    pub(super) fn event(fixture: EventFixture<'_>) -> UsageEvent {
        UsageEvent {
            event_id: fixture.event_id.to_string(),
            occurred_at_ms: fixture.occurred_at_ms,
            account_identifier: fixture.account.to_string(),
            provider: fixture.provider.to_string(),
            model: fixture.model.to_string(),
            key_identifier: Some(fixture.key.to_string()),
            status_code: fixture.status_code,
            succeeded: fixture.status_code < 400,
            input_tokens: fixture.input_tokens,
            output_tokens: fixture.output_tokens,
            cached_input_tokens: fixture.cached_input_tokens,
            reasoning_tokens: fixture.output_tokens / 2,
            total_tokens: fixture.input_tokens + fixture.output_tokens,
            latency_ms: 125,
        }
    }

    pub(super) fn price(
        model: &str,
        version: &str,
        input: f64,
        output: f64,
        cached: f64,
        effective_from_ms: i64,
    ) -> ModelPrice {
        ModelPrice {
            model: model.to_string(),
            version: version.to_string(),
            input_per_million: input,
            output_per_million: output,
            cached_input_per_million: cached,
            effective_from_ms,
        }
    }

    pub(super) fn assert_cost(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 0.000_001,
            "cost was {actual}, expected {expected}"
        );
    }

    #[test]
    fn fixture_roundtrip_all_dimensions() {
        let path = TestPath::new("fixture");
        let history = ready(&path.0);
        history
            .set_model_price(&price("gpt", "gpt-v1", 2.0, 4.0, 1.0, 0))
            .unwrap();
        history
            .set_model_price(&price("opus", "opus-v1", 3.0, 6.0, 0.5, 0))
            .unwrap();
        let fixtures = [
            event(EventFixture {
                event_id: "event-a",
                occurred_at_ms: 3_660_000,
                account: "alice",
                provider: "codex",
                model: "gpt",
                key: "key-a",
                status_code: 200,
                input_tokens: 1_000_000,
                output_tokens: 100_000,
                cached_input_tokens: 200_000,
            }),
            event(EventFixture {
                event_id: "event-b",
                occurred_at_ms: 3_720_000,
                account: "alice",
                provider: "codex",
                model: "gpt",
                key: "key-a",
                status_code: 500,
                input_tokens: 500_000,
                output_tokens: 50_000,
                cached_input_tokens: 0,
            }),
            event(EventFixture {
                event_id: "event-c",
                occurred_at_ms: 7_260_000,
                account: "bob",
                provider: "claude",
                model: "opus",
                key: "key-b",
                status_code: 429,
                input_tokens: 2_000_000,
                output_tokens: 100_000,
                cached_input_tokens: 1_000_000,
            }),
        ];
        for fixture in &fixtures {
            assert!(history.insert(fixture).unwrap());
        }

        let totals = history.totals().unwrap();
        assert_eq!(totals.requests, 3);
        assert_eq!(totals.successful_requests, 1);
        assert_eq!(totals.failed_requests, 2);
        assert_eq!(totals.input_tokens, 3_500_000);
        assert_eq!(totals.output_tokens, 250_000);
        assert_eq!(totals.cached_input_tokens, 1_200_000);
        assert_eq!(totals.reasoning_tokens, 125_000);
        assert_eq!(totals.total_tokens, 3_750_000);
        assert_cost(totals.estimated_cost_usd, 7.5);

        let grouped = history
            .query(&HistoryQuery {
                time_bucket: Some(TimeBucket::Hour),
                group_by: vec![
                    GroupDimension::Account,
                    GroupDimension::Provider,
                    GroupDimension::Model,
                    GroupDimension::Key,
                    GroupDimension::Status,
                ],
                ..HistoryQuery::default()
            })
            .unwrap();
        assert_eq!(grouped.groups.len(), 3);
        assert_eq!(grouped.groups[0].key.bucket_start_ms, Some(3_600_000));
        assert_eq!(grouped.groups[0].key.account.as_deref(), Some("alice"));
        assert_eq!(grouped.groups[0].key.provider.as_deref(), Some("codex"));
        assert_eq!(grouped.groups[0].key.model.as_deref(), Some("gpt"));
        assert_eq!(
            grouped.groups[0].key.key_identifier.as_deref(),
            Some("key-a")
        );
        assert_eq!(grouped.groups[0].key.status_code, Some(200));
        assert_eq!(grouped.groups[0].totals.input_tokens, 1_000_000);
        assert_cost(grouped.groups[0].totals.estimated_cost_usd, 2.2);
        assert_eq!(grouped.groups[1].key.status_code, Some(500));
        assert_eq!(grouped.groups[1].totals.output_tokens, 50_000);
        assert_cost(grouped.groups[1].totals.estimated_cost_usd, 1.2);
        assert_eq!(grouped.groups[2].key.bucket_start_ms, Some(7_200_000));
        assert_eq!(grouped.groups[2].key.account.as_deref(), Some("bob"));
        assert_eq!(grouped.groups[2].key.status_code, Some(429));
        assert_cost(grouped.groups[2].totals.estimated_cost_usd, 4.1);

        let filtered = history
            .query(&HistoryQuery {
                start_ms: Some(3_700_000),
                end_ms: Some(7_300_000),
                accounts: vec!["alice".to_string()],
                providers: vec!["codex".to_string()],
                models: vec!["gpt".to_string()],
                key_identifiers: vec!["key-a".to_string()],
                status_codes: vec![500],
                outcomes: vec![OutcomeFilter::Failed],
                ..HistoryQuery::default()
            })
            .unwrap();
        assert_eq!(filtered.totals.requests, 1);
        assert_eq!(filtered.totals.input_tokens, 500_000);
        assert_cost(filtered.totals.estimated_cost_usd, 1.2);
    }

    #[test]
    fn duplicate_event_id_is_idempotent() {
        let path = TestPath::new("duplicate");
        let history = ready(&path.0);
        let event = event(EventFixture {
            event_id: "same-event",
            occurred_at_ms: 1,
            account: "a",
            provider: "p",
            model: "m",
            key: "key",
            status_code: 200,
            input_tokens: 10,
            output_tokens: 5,
            cached_input_tokens: 0,
        });
        assert!(history.insert(&event).unwrap());
        assert!(!history.insert(&event).unwrap());
        assert_eq!(history.totals().unwrap().requests, 1);
    }

    #[test]
    fn invalid_bucket_is_typed_error() {
        let error = TimeBucket::parse("fortnight").expect_err("invalid bucket must fail");
        assert_eq!(
            error,
            HistoryError::InvalidTimeBucket("fortnight".to_string())
        );
    }

    #[test]
    fn corrupt_database_opens_in_explicit_degraded_state() {
        let path = TestPath::new("corrupt");
        std::fs::write(&path.0, b"not a sqlite database").unwrap();
        match RequestHistory::open(&path.0) {
            HistoryState::Degraded { error } => {
                assert!(
                    error.contains("database") || error.contains("SQLite"),
                    "{error}"
                )
            }
            HistoryState::Ready(_) => panic!("corruption must produce a degraded state"),
        }
    }
}

#[cfg(test)]
mod extended_tests {
    use super::tests::{
        assert_cost, event, price, ready, ready_with_config, EventFixture, TestPath,
    };
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::time::Instant;

    #[test]
    fn open_failure_is_an_explicit_degraded_state() {
        let path = TestPath::new("open-failure");
        std::fs::create_dir(&path.0).unwrap();
        match RequestHistory::open(&path.0) {
            HistoryState::Degraded { error } => assert!(!error.is_empty()),
            HistoryState::Ready(_) => panic!("directory path must not open as a database"),
        }
        std::fs::remove_dir(&path.0).unwrap();
    }

    #[test]
    fn migrations_open_empty_current_and_previous_version_databases() {
        let empty = TestPath::new("migration-empty");
        let empty_history = ready(&empty.0);
        assert_eq!(empty_history.schema_version().unwrap(), SCHEMA_VERSION);
        drop(empty_history);

        let current_history = ready(&empty.0);
        assert_eq!(current_history.schema_version().unwrap(), SCHEMA_VERSION);
        drop(current_history);

        let previous = TestPath::new("migration-previous");
        let previous_connection = Connection::open(&previous.0).unwrap();
        previous_connection.execute_batch(MIGRATION_V1).unwrap();
        previous_connection
            .execute(
                "INSERT INTO model_prices(model, input_per_million, output_per_million, cached_input_per_million, updated_at_ms)
                 VALUES ('legacy-model', 1.0, 2.0, 0.5, 1)",
                [],
            )
            .unwrap();
        previous_connection
            .pragma_update(None, "user_version", 1)
            .unwrap();
        drop(previous_connection);

        let migrated = ready(&previous.0);
        assert_eq!(migrated.schema_version().unwrap(), SCHEMA_VERSION);
        migrated
            .insert(&event(EventFixture {
                event_id: "legacy-priced",
                occurred_at_ms: 10,
                account: "a",
                provider: "p",
                model: "legacy-model",
                key: "k",
                status_code: 200,
                input_tokens: 1_000_000,
                output_tokens: 0,
                cached_input_tokens: 0,
            }))
            .unwrap();
        assert_cost(migrated.totals().unwrap().estimated_cost_usd, 1.0);
    }

    #[test]
    fn estimated_cost_is_snapshotted_then_explicitly_recomputed() {
        let path = TestPath::new("recompute");
        let history = ready(&path.0);
        history
            .set_model_price(&price("m", "v1", 1.0, 2.0, 0.5, 0))
            .unwrap();
        history
            .insert(&event(EventFixture {
                event_id: "priced",
                occurred_at_ms: 1_000,
                account: "a",
                provider: "p",
                model: "m",
                key: "k",
                status_code: 200,
                input_tokens: 1_000_000,
                output_tokens: 100_000,
                cached_input_tokens: 200_000,
            }))
            .unwrap();
        assert_cost(history.totals().unwrap().estimated_cost_usd, 1.1);

        history
            .set_model_price(&price("m", "v2", 10.0, 20.0, 5.0, 500))
            .unwrap();
        assert_cost(history.totals().unwrap().estimated_cost_usd, 1.1);
        assert_eq!(history.recompute_estimated_costs().unwrap(), 1);
        assert_cost(history.totals().unwrap().estimated_cost_usd, 11.0);
    }

    #[test]
    fn explain_query_plan_uses_composite_filter_indexes() {
        let path = TestPath::new("indexes");
        let history = ready(&path.0);
        let cases = [
            (
                HistoryQuery {
                    start_ms: Some(0),
                    end_ms: Some(10),
                    accounts: vec!["a".to_string()],
                    ..HistoryQuery::default()
                },
                "idx_usage_events_account_time",
            ),
            (
                HistoryQuery {
                    start_ms: Some(0),
                    end_ms: Some(10),
                    providers: vec!["p".to_string()],
                    ..HistoryQuery::default()
                },
                "idx_usage_events_provider_time",
            ),
            (
                HistoryQuery {
                    start_ms: Some(0),
                    end_ms: Some(10),
                    models: vec!["m".to_string()],
                    ..HistoryQuery::default()
                },
                "idx_usage_events_model_time",
            ),
            (
                HistoryQuery {
                    start_ms: Some(0),
                    end_ms: Some(10),
                    key_identifiers: vec!["k".to_string()],
                    ..HistoryQuery::default()
                },
                "idx_usage_events_key_time",
            ),
            (
                HistoryQuery {
                    start_ms: Some(0),
                    end_ms: Some(10),
                    status_codes: vec![429],
                    ..HistoryQuery::default()
                },
                "idx_usage_events_status_time",
            ),
        ];
        for (query, expected_index) in cases {
            let plan = history.explain_query_plan(&query).unwrap().join("\n");
            assert!(
                plan.contains(expected_index),
                "plan did not use {expected_index}: {plan}"
            );
        }
    }

    #[test]
    fn retention_and_size_pruning_are_chunked() {
        let path = TestPath::new("prune");
        let history = ready(&path.0);
        for index in 0..12 {
            let account = format!("account-{index}-{}", "x".repeat(4096));
            history
                .insert(&event(EventFixture {
                    event_id: &format!("event-{index}"),
                    occurred_at_ms: index * 1_000,
                    account: &account,
                    provider: "provider",
                    model: "model",
                    key: "key",
                    status_code: 200,
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: 0,
                }))
                .unwrap();
        }
        let retention = history
            .prune_with_policy(
                10_000,
                PrunePolicy {
                    retention: Some(Duration::from_millis(5_000)),
                    max_size_bytes: None,
                    chunk_size: 2,
                },
            )
            .unwrap();
        assert_eq!(retention.deleted_events, 5);
        assert_eq!(retention.chunks, 3);
        assert_eq!(history.totals().unwrap().requests, 7);

        let baseline_path = TestPath::new("prune-baseline");
        let baseline = ready(&baseline_path.0)
            .prune_with_policy(
                0,
                PrunePolicy {
                    retention: None,
                    max_size_bytes: None,
                    chunk_size: 1,
                },
            )
            .unwrap()
            .logical_size_bytes;
        let size = history
            .prune_with_policy(
                10_000,
                PrunePolicy {
                    retention: None,
                    max_size_bytes: Some(baseline + 8_192),
                    chunk_size: 2,
                },
            )
            .unwrap();
        assert!(size.deleted_events > 0);
        assert!(size.chunks > 0);
        assert!(
            size.size_cap_satisfied,
            "logical size was {}",
            size.logical_size_bytes
        );
    }

    #[test]
    fn busy_timeout_is_bounded() {
        let path = TestPath::new("busy");
        let history = ready_with_config(
            &path.0,
            HistoryConfig {
                busy_timeout: Duration::from_millis(40),
                retention: None,
                max_size_bytes: None,
                prune_chunk_size: 10,
            },
        );
        let locker = Connection::open(&path.0).unwrap();
        locker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = Instant::now();
        let error = history
            .insert(&event(EventFixture {
                event_id: "busy",
                occurred_at_ms: 1,
                account: "a",
                provider: "p",
                model: "m",
                key: "k",
                status_code: 200,
                input_tokens: 1,
                output_tokens: 1,
                cached_input_tokens: 0,
            }))
            .expect_err("competing writer must hit the configured timeout");
        let elapsed = started.elapsed();
        assert!(matches!(error, HistoryError::Database(_)));
        assert!(
            elapsed < Duration::from_millis(500),
            "busy wait was {elapsed:?}"
        );
        locker.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn concurrent_prune_and_read_complete_on_the_worker() {
        let path = TestPath::new("concurrent");
        let history = ready(&path.0);
        for index in 0..100 {
            history
                .insert(&event(EventFixture {
                    event_id: &format!("event-{index}"),
                    occurred_at_ms: index,
                    account: "a",
                    provider: "p",
                    model: "m",
                    key: "k",
                    status_code: 200,
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: 0,
                }))
                .unwrap();
        }
        let barrier = Arc::new(Barrier::new(3));
        let prune_history = history.clone();
        let prune_barrier = Arc::clone(&barrier);
        let prune = std::thread::spawn(move || {
            prune_barrier.wait();
            prune_history.prune_with_policy(
                1_000,
                PrunePolicy {
                    retention: Some(Duration::from_millis(950)),
                    max_size_bytes: None,
                    chunk_size: 7,
                },
            )
        });
        let read_history = history.clone();
        let read_barrier = Arc::clone(&barrier);
        let read = std::thread::spawn(move || {
            read_barrier.wait();
            read_history.totals()
        });
        barrier.wait();
        let pruned = prune.join().expect("prune thread").expect("prune result");
        let totals = read.join().expect("read thread").expect("read result");
        assert_eq!(pruned.deleted_events, 50);
        assert!(totals.requests == 100 || totals.requests == 50);
        assert_eq!(history.totals().unwrap().requests, 50);
    }

    #[test]
    fn metadata_imports_and_key_identifier_are_non_secret_and_idempotent() {
        assert_eq!(
            stable_key_identifier("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let path = TestPath::new("metadata");
        let history = ready(&path.0);
        history.set_metadata("collector", "v1").unwrap();
        assert_eq!(
            history.metadata("collector").unwrap().as_deref(),
            Some("v1")
        );
        let record = ImportRecord {
            import_id: "import-1".to_string(),
            source: "request-events-jsonl".to_string(),
            imported_at_ms: 1,
            event_count: 2,
        };
        assert!(history.record_import(&record).unwrap());
        assert!(!history.record_import(&record).unwrap());

        let secret = "raw-inbound-key-do-not-store";
        history
            .insert(&UsageEvent {
                key_identifier: Some(stable_key_identifier(secret)),
                ..event(EventFixture {
                    event_id: "secret-check",
                    occurred_at_ms: 1,
                    account: "a",
                    provider: "p",
                    model: "m",
                    key: "unused",
                    status_code: 200,
                    input_tokens: 1,
                    output_tokens: 1,
                    cached_input_tokens: 0,
                })
            })
            .unwrap();
        drop(history);
        let database_bytes = std::fs::read(&path.0).unwrap();
        assert!(!database_bytes
            .windows(secret.len())
            .any(|window| window == secret.as_bytes()));
    }
}

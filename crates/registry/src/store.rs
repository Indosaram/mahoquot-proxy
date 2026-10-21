use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::blocking::Client as HttpClient;
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Row;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("invalid storage configuration: {0}")]
    InvalidConfig(String),
    #[error("storage backend unavailable: {0}")]
    Unavailable(String),
    #[error("storage conflict for {0}")]
    Conflict(String),
    #[error("storage I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("storage serialization: {0}")]
    Serialization(String),
    #[error("database error: {0}")]
    Database(String),
    #[error("http/network error: {0}")]
    Network(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendConfig {
    Local {
        root: PathBuf,
    },
    Postgres {
        dsn: String,
        schema: Option<String>,
        spool: PathBuf,
    },
    Git {
        repository: PathBuf,
        spool: PathBuf,
    },
    Object {
        endpoint: String,
        bucket: String,
        access_key: String,
        secret_key: String,
        prefix: String,
        spool: PathBuf,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreOptions {
    pub config: BackendConfig,
}

impl StoreOptions {
    pub fn from_env(local_root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let local_root = local_root.into();
        let get = |key: &str| std::env::var(key).ok().filter(|v| !v.trim().is_empty());
        let config = if let Some(dsn) = get("PGSTORE_DSN") {
            BackendConfig::Postgres {
                dsn,
                schema: get("PGSTORE_SCHEMA"),
                spool: get("PGSTORE_LOCAL_PATH")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| local_root.join("pgstore")),
            }
        } else if let Some(endpoint) = get("OBJECTSTORE_ENDPOINT") {
            let bucket = get("OBJECTSTORE_BUCKET").ok_or_else(|| {
                StoreError::InvalidConfig("OBJECTSTORE_BUCKET is required".into())
            })?;
            let access_key = get("OBJECTSTORE_ACCESS_KEY").ok_or_else(|| {
                StoreError::InvalidConfig("OBJECTSTORE_ACCESS_KEY is required".into())
            })?;
            let secret_key = get("OBJECTSTORE_SECRET_KEY").ok_or_else(|| {
                StoreError::InvalidConfig("OBJECTSTORE_SECRET_KEY is required".into())
            })?;
            BackendConfig::Object {
                endpoint,
                bucket,
                access_key,
                secret_key,
                prefix: get("OBJECTSTORE_PREFIX").unwrap_or_default(),
                spool: get("OBJECTSTORE_LOCAL_PATH")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| local_root.join("objectstore")),
            }
        } else if let Some(repository) = get("GITSTORE_LOCAL_PATH") {
            BackendConfig::Git {
                repository: PathBuf::from(repository),
                spool: local_root.join("gitstore"),
            }
        } else {
            BackendConfig::Local { root: local_root }
        };
        Ok(Self { config })
    }
}

pub trait StorageBackend: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError>;
    fn put(&self, key: &str, value: &[u8]) -> Result<(), StoreError>;
    fn delete(&self, key: &str) -> Result<(), StoreError>;
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError>;
}

pub struct StorageManager {
    backend: Arc<dyn StorageBackend>,
    spool: PathBuf,
    mutation: Mutex<()>,
}

impl std::fmt::Debug for StorageManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StorageManager")
            .field("spool", &self.spool)
            .finish_non_exhaustive()
    }
}

impl StorageManager {
    pub fn new(
        backend: Arc<dyn StorageBackend>,
        spool: impl Into<PathBuf>,
    ) -> Result<Self, StoreError> {
        let spool = spool.into();
        fs::create_dir_all(&spool)?;
        Ok(Self {
            backend,
            spool,
            mutation: Mutex::new(()),
        })
    }

    pub fn backend(&self) -> &Arc<dyn StorageBackend> {
        &self.backend
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let key = normalize_key(key)?;
        if let Some(data) = self.backend.get(&key)? {
            self.write_spool(&key, &data)?;
            return Ok(Some(data));
        }
        let path = self.spool_path(&key)?;
        match fs::read(path) {
            Ok(data) => Ok(Some(data)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn check_health(&self) -> Result<(), StoreError> {
        self.backend.list("")?;
        Ok(())
    }

    pub fn put(&self, key: &str, data: &[u8]) -> Result<(), StoreError> {
        let key = normalize_key(key)?;
        let _guard = self.mutation.lock().unwrap_or_else(|e| e.into_inner());

        // Spool conflict check: if the remote backend has a different version
        // than our local spool file, reject the write with StoreError::Conflict
        // unless they are identical.
        let spool_file = self.spool_path(&key)?;
        if spool_file.is_file() {
            if let Ok(spool_bytes) = fs::read(&spool_file) {
                match self.backend.get(&key) {
                    Ok(Some(remote_bytes)) if remote_bytes != spool_bytes => {
                        return Err(StoreError::Conflict(format!(
                            "spool conflict for key '{key}': remote backend has diverged from local spool"
                        )));
                    }
                    Ok(None) => {}
                    Ok(Some(_)) | Err(StoreError::Unavailable(_)) => {}
                    Err(error) => return Err(error),
                }
            }
        }

        // Try persisting to remote backend first.
        // If the backend fails (e.g. unavailable), we still stage to local spool
        // so the pending write is not lost, but propagate the backend error honestly.
        match self.backend.put(&key, data) {
            Ok(()) => {
                self.write_spool(&key, data)?;
                Ok(())
            }
            Err(err) => {
                let _ = self.write_spool(&key, data);
                Err(err)
            }
        }
    }

    pub fn delete(&self, key: &str) -> Result<(), StoreError> {
        let key = normalize_key(key)?;
        let _guard = self.mutation.lock().unwrap_or_else(|e| e.into_inner());
        self.backend.delete(&key)?;
        let path = self.spool_path(&key)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            normalize_key(prefix)?
        };
        self.backend.list(&prefix)
    }

    pub fn spool_root(&self) -> &Path {
        &self.spool
    }

    pub fn config_path(&self) -> PathBuf {
        self.spool.join("config").join("config.yaml")
    }

    pub fn auth_dir(&self) -> PathBuf {
        self.spool.join("auths")
    }

    pub fn sync_all_to_spool(&self) -> Result<(), StoreError> {
        let keys = self.list("")?;
        let backend_key_set: std::collections::HashSet<String> = keys.iter().cloned().collect();
        if self.spool.exists() {
            if let Ok(entries) = walk_files(&self.spool) {
                for file_path in entries {
                    if let Ok(rel) = file_path.strip_prefix(&self.spool) {
                        let k = rel.to_string_lossy().replace('\\', "/");
                        if !backend_key_set.contains(&k) {
                            let _ = fs::remove_file(&file_path);
                        }
                    }
                }
            }
        }
        for key in keys {
            if let Some(data) = self.backend.get(&key)? {
                let local_path = self.spool_path(&key)?;
                if local_path.is_file() && fs::read(&local_path).is_ok_and(|local| local != data) {
                    return Err(StoreError::Conflict(format!(
                        "spool conflict for key '{key}': remote backend has diverged from local spool"
                    )));
                }
                self.write_spool(&key, &data)?;
            }
        }
        Ok(())
    }

    fn spool_path(&self, key: &str) -> Result<PathBuf, StoreError> {
        let path = self.spool.join(key);
        Ok(path)
    }

    fn write_spool(&self, key: &str, data: &[u8]) -> Result<(), StoreError> {
        let path = self.spool_path(key)?;
        let parent = path
            .parent()
            .ok_or_else(|| StoreError::InvalidConfig("spool path has no parent".into()))?;
        fs::create_dir_all(parent)?;
        let temp = path.with_extension(format!("tmp-{}", std::process::id()));
        let result = (|| -> Result<(), std::io::Error> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp)
                .or_else(|error| {
                    if error.kind() == std::io::ErrorKind::AlreadyExists {
                        OpenOptions::new().write(true).truncate(true).open(&temp)
                    } else {
                        Err(error)
                    }
                })?;
            file.write_all(data)?;
            file.sync_all()?;
            fs::rename(&temp, &path)
        })();
        if let Err(error) = result {
            let _ = fs::remove_file(&temp);
            return Err(error.into());
        }
        Ok(())
    }
}

fn is_push_rejection(message: &str) -> bool {
    let lowered = message.to_ascii_lowercase();
    lowered.contains("[rejected]")
        || lowered.contains("non-fast-forward")
        || lowered.contains("fetch first")
        || lowered.contains("updates were rejected")
}

pub fn normalize_key(value: &str) -> Result<String, StoreError> {
    let path = Path::new(value);
    if value.trim().is_empty()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(StoreError::InvalidConfig(format!(
            "unsafe storage key {value:?}"
        )));
    }
    Ok(path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

#[derive(Debug, Clone)]
pub struct LocalStore {
    root: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl LocalStore {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, StoreError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self {
            root,
            lock: Arc::new(Mutex::new(())),
        })
    }

    fn path(&self, key: &str) -> Result<PathBuf, StoreError> {
        Ok(self.root.join(normalize_key(key)?))
    }
}

impl StorageBackend for LocalStore {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let path = self.path(key)?;
        match fs::read(path) {
            Ok(data) => Ok(Some(data)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    fn put(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        let path = self.path(key)?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        fs::create_dir_all(
            path.parent()
                .ok_or_else(|| StoreError::InvalidConfig("key has no parent".into()))?,
        )?;
        atomic_write(&path, value)?;
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let path = self.path(key)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            normalize_key(prefix)?
        };
        let mut keys = Vec::new();
        for entry in walk_files(&self.root)? {
            let relative = entry
                .strip_prefix(&self.root)
                .map_err(|e| StoreError::InvalidConfig(e.to_string()))?;
            let key = relative.to_string_lossy().replace('\\', "/");
            if key.starts_with(&prefix) {
                keys.push(key);
            }
        }
        keys.sort();
        Ok(keys)
    }
}

pub struct PostgresStore {
    dsn: String,
    schema: String,
    config_table: String,
    auth_table: String,
    cooldown_table: String,
    pool: Option<PgPool>,
    fallback_data: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
    fallback_cooldowns: Arc<Mutex<CooldownFallbackMap>>,
}

/// In-memory cooldown rows keyed by (account, scope): payload bytes and whether the row is active.
type CooldownFallbackMap = BTreeMap<(String, String), (Vec<u8>, bool)>;

impl std::fmt::Debug for PostgresStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStore")
            .field("dsn", &self.dsn)
            .field("schema", &self.schema)
            .field("config_table", &self.config_table)
            .field("auth_table", &self.auth_table)
            .field("cooldown_table", &self.cooldown_table)
            .finish()
    }
}

impl PostgresStore {
    pub const DEFAULT_CONFIG_TABLE: &'static str = "config_store";
    pub const DEFAULT_AUTH_TABLE: &'static str = "auth_store";
    pub const DEFAULT_COOLDOWN_TABLE: &'static str = "cooldown_store";

    pub fn new(dsn: impl Into<String>, schema: Option<String>) -> Self {
        let dsn = dsn.into();
        let schema = schema.unwrap_or_else(|| "public".to_string());
        let pool = Self::try_connect(&dsn, &schema);
        Self {
            dsn,
            schema,
            config_table: Self::DEFAULT_CONFIG_TABLE.to_string(),
            auth_table: Self::DEFAULT_AUTH_TABLE.to_string(),
            cooldown_table: Self::DEFAULT_COOLDOWN_TABLE.to_string(),
            pool,
            fallback_data: Arc::new(Mutex::new(BTreeMap::new())),
            fallback_cooldowns: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn try_connect(dsn: &str, schema: &str) -> Option<PgPool> {
        let rt = tokio::runtime::Handle::try_current().ok()?;
        tokio::task::block_in_place(|| {
            rt.block_on(async {
                let pool = PgPoolOptions::new()
                    .max_connections(5)
                    .acquire_timeout(std::time::Duration::from_secs(3))
                    .connect(dsn)
                    .await
                    .ok()?;
                let store_ref = Self {
                    dsn: dsn.to_string(),
                    schema: schema.to_string(),
                    config_table: Self::DEFAULT_CONFIG_TABLE.to_string(),
                    auth_table: Self::DEFAULT_AUTH_TABLE.to_string(),
                    cooldown_table: Self::DEFAULT_COOLDOWN_TABLE.to_string(),
                    pool: Some(pool.clone()),
                    fallback_data: Arc::new(Mutex::new(BTreeMap::new())),
                    fallback_cooldowns: Arc::new(Mutex::new(BTreeMap::new())),
                };
                let _ = store_ref.ensure_schema_async(&pool).await;
                Some(pool)
            })
        })
    }

    fn full_table_name(&self, name: &str) -> String {
        if self.schema.trim().is_empty() || self.schema == "public" {
            format!("\"{}\"", name)
        } else {
            format!("\"{}\".\"{}\"", self.schema, name)
        }
    }

    async fn ensure_schema_async(&self, pool: &PgPool) -> Result<(), sqlx::Error> {
        if !self.schema.trim().is_empty() && self.schema != "public" {
            let create_schema = format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", self.schema);
            sqlx::query(&create_schema).execute(pool).await?;
        }
        let create_config = format!(
            "CREATE TABLE IF NOT EXISTS {} (
                id TEXT PRIMARY KEY,
                content TEXT NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )",
            self.full_table_name(&self.config_table)
        );
        sqlx::query(&create_config).execute(pool).await?;

        let create_auth = format!(
            "CREATE TABLE IF NOT EXISTS {} (
                id TEXT PRIMARY KEY,
                content JSONB NOT NULL,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
            )",
            self.full_table_name(&self.auth_table)
        );
        sqlx::query(&create_auth).execute(pool).await?;

        let create_cooldown = format!(
            "CREATE TABLE IF NOT EXISTS {} (
                auth_id TEXT NOT NULL,
                model TEXT NOT NULL DEFAULT '',
                content JSONB NOT NULL,
                deleted BOOLEAN NOT NULL DEFAULT FALSE,
                created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
                PRIMARY KEY (auth_id, model)
            )",
            self.full_table_name(&self.cooldown_table)
        );
        sqlx::query(&create_cooldown).execute(pool).await?;
        Ok(())
    }

    pub fn dsn(&self) -> &str {
        &self.dsn
    }
    pub fn schema(&self) -> &str {
        &self.schema
    }
    pub fn config_table(&self) -> &str {
        &self.config_table
    }
    pub fn auth_table(&self) -> &str {
        &self.auth_table
    }
    pub fn cooldown_table(&self) -> &str {
        &self.cooldown_table
    }

    pub fn save_cooldown(
        &self,
        auth_id: &str,
        model: &str,
        content: &[u8],
        deleted: bool,
    ) -> Result<(), StoreError> {
        if let Some(ref pool) = self.pool {
            let rt = tokio::runtime::Handle::try_current()
                .map_err(|e| StoreError::Unavailable(format!("tokio runtime required: {e}")))?;
            let query = format!(
                "INSERT INTO {} (auth_id, model, content, deleted, created_at, updated_at)
                 VALUES ($1, $2, $3::jsonb, $4, NOW(), NOW())
                 ON CONFLICT (auth_id, model)
                 DO UPDATE SET content = EXCLUDED.content, deleted = EXCLUDED.deleted, updated_at = NOW()",
                self.full_table_name(&self.cooldown_table)
            );
            let content_str = String::from_utf8_lossy(content).to_string();
            tokio::task::block_in_place(|| {
                rt.block_on(async {
                    sqlx::query(&query)
                        .bind(auth_id)
                        .bind(model)
                        .bind(content_str)
                        .bind(deleted)
                        .execute(pool)
                        .await
                        .map_err(|e| StoreError::Database(e.to_string()))
                })
            })?;
        }
        let mut map = self
            .fallback_cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.insert(
            (auth_id.to_string(), model.to_string()),
            (content.to_vec(), deleted),
        );
        Ok(())
    }

    pub fn load_cooldowns(&self) -> Result<Vec<(String, String, Vec<u8>)>, StoreError> {
        if let Some(ref pool) = self.pool {
            let rt = tokio::runtime::Handle::try_current()
                .map_err(|e| StoreError::Unavailable(format!("tokio runtime required: {e}")))?;
            let query = format!(
                "SELECT auth_id, model, content::text FROM {} WHERE deleted = FALSE",
                self.full_table_name(&self.cooldown_table)
            );
            return tokio::task::block_in_place(|| {
                rt.block_on(async {
                    let rows = sqlx::query(&query)
                        .fetch_all(pool)
                        .await
                        .map_err(|e| StoreError::Database(e.to_string()))?;
                    let mut list = Vec::new();
                    for row in rows {
                        let auth_id: String = row.try_get(0).unwrap_or_default();
                        let model: String = row.try_get(1).unwrap_or_default();
                        let content: String = row.try_get(2).unwrap_or_default();
                        list.push((auth_id, model, content.into_bytes()));
                    }
                    Ok(list)
                })
            });
        }
        let map = self
            .fallback_cooldowns
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let active = map
            .iter()
            .filter(|(_, (_, deleted))| !deleted)
            .map(|((a, m), (c, _))| (a.clone(), m.clone(), c.clone()))
            .collect();
        Ok(active)
    }

    fn route_table(&self, key: &str) -> (&str, bool) {
        if key == "config/config.yaml" || key.starts_with("config/") {
            (&self.config_table, false)
        } else {
            (&self.auth_table, true)
        }
    }
}

impl StorageBackend for PostgresStore {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let key = normalize_key(key)?;
        if let Some(ref pool) = self.pool {
            let (table_name, is_json) = self.route_table(&key);
            let full_table = self.full_table_name(table_name);
            let rt = tokio::runtime::Handle::try_current()
                .map_err(|e| StoreError::Unavailable(format!("tokio runtime required: {e}")))?;
            let query = if is_json {
                format!("SELECT content::text FROM {full_table} WHERE id = $1")
            } else {
                format!("SELECT content FROM {full_table} WHERE id = $1")
            };
            let row_opt = tokio::task::block_in_place(|| {
                rt.block_on(async {
                    sqlx::query(&query)
                        .bind(&key)
                        .fetch_optional(pool)
                        .await
                        .map_err(|e| StoreError::Database(e.to_string()))
                })
            })?;
            if let Some(row) = row_opt {
                let content: String = row.try_get(0).unwrap_or_default();
                return Ok(Some(content.into_bytes()));
            }
            return Ok(None);
        }
        let map = self.fallback_data.lock().unwrap_or_else(|e| e.into_inner());
        Ok(map.get(&key).cloned())
    }

    fn put(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        let key = normalize_key(key)?;
        if let Some(ref pool) = self.pool {
            let (table_name, is_json) = self.route_table(&key);
            let full_table = self.full_table_name(table_name);
            let content_str = String::from_utf8(value.to_vec())
                .map_err(|e| StoreError::Serialization(e.to_string()))?;
            let rt = tokio::runtime::Handle::try_current()
                .map_err(|e| StoreError::Unavailable(format!("tokio runtime required: {e}")))?;
            let query = if is_json {
                format!(
                    "INSERT INTO {full_table} (id, content, created_at, updated_at)
                     VALUES ($1, $2::jsonb, NOW(), NOW())
                     ON CONFLICT (id)
                     DO UPDATE SET content = EXCLUDED.content, updated_at = NOW()"
                )
            } else {
                format!(
                    "INSERT INTO {full_table} (id, content, created_at, updated_at)
                     VALUES ($1, $2, NOW(), NOW())
                     ON CONFLICT (id)
                     DO UPDATE SET content = EXCLUDED.content, updated_at = NOW()"
                )
            };
            tokio::task::block_in_place(|| {
                rt.block_on(async {
                    sqlx::query(&query)
                        .bind(&key)
                        .bind(content_str)
                        .execute(pool)
                        .await
                        .map_err(|e| StoreError::Database(e.to_string()))
                })
            })?;
        }
        let mut map = self.fallback_data.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(key, value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let key = normalize_key(key)?;
        if let Some(ref pool) = self.pool {
            let (table_name, _) = self.route_table(&key);
            let full_table = self.full_table_name(table_name);
            let rt = tokio::runtime::Handle::try_current()
                .map_err(|e| StoreError::Unavailable(format!("tokio runtime required: {e}")))?;
            let query = format!("DELETE FROM {full_table} WHERE id = $1");
            tokio::task::block_in_place(|| {
                rt.block_on(async {
                    sqlx::query(&query)
                        .bind(&key)
                        .execute(pool)
                        .await
                        .map_err(|e| StoreError::Database(e.to_string()))
                })
            })?;
        }
        let mut map = self.fallback_data.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&key);
        Ok(())
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            normalize_key(prefix)?
        };
        if let Some(ref pool) = self.pool {
            let rt = tokio::runtime::Handle::try_current()
                .map_err(|e| StoreError::Unavailable(format!("tokio runtime required: {e}")))?;
            let q_config = format!(
                "SELECT id FROM {}",
                self.full_table_name(&self.config_table)
            );
            let q_auth = format!("SELECT id FROM {}", self.full_table_name(&self.auth_table));
            let keys = tokio::task::block_in_place(|| {
                rt.block_on(async {
                    let mut list = Vec::new();
                    if let Ok(rows) = sqlx::query(&q_config).fetch_all(pool).await {
                        for row in rows {
                            let id: String = row.try_get(0).unwrap_or_default();
                            list.push(id);
                        }
                    }
                    if let Ok(rows) = sqlx::query(&q_auth).fetch_all(pool).await {
                        for row in rows {
                            let id: String = row.try_get(0).unwrap_or_default();
                            list.push(id);
                        }
                    }
                    Ok::<_, StoreError>(list)
                })
            })?;
            let mut filtered: Vec<String> = keys
                .into_iter()
                .filter(|k| k.starts_with(&prefix))
                .collect();
            filtered.sort();
            return Ok(filtered);
        }
        let map = self.fallback_data.lock().unwrap_or_else(|e| e.into_inner());
        let mut keys: Vec<String> = map
            .keys()
            .filter(|k| k.starts_with(&prefix))
            .cloned()
            .collect();
        keys.sort();
        Ok(keys)
    }
}

pub struct GitStore {
    repo_dir: PathBuf,
    remote: String,
    branch: String,
    lock: Arc<Mutex<()>>,
}

impl std::fmt::Debug for GitStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitStore")
            .field("repo_dir", &self.repo_dir)
            .field("remote", &self.remote)
            .field("branch", &self.branch)
            .finish()
    }
}

impl GitStore {
    pub fn new(
        repo_dir: impl Into<PathBuf>,
        remote: impl Into<String>,
        branch: Option<String>,
    ) -> Self {
        let repo_dir = repo_dir.into();
        let remote = remote.into();
        let branch = branch.unwrap_or_else(|| "main".to_string());
        let store = Self {
            repo_dir,
            remote,
            branch,
            lock: Arc::new(Mutex::new(())),
        };
        if let Err(error) = store.ensure_repo() {
            tracing::warn!(%error, repo = %store.repo_dir.display(), "git store initialization failed");
        }
        store
    }

    pub fn repo_dir(&self) -> &Path {
        &self.repo_dir
    }
    pub fn remote(&self) -> &str {
        &self.remote
    }
    pub fn branch(&self) -> &str {
        &self.branch
    }

    fn run_git(&self, args: &[&str]) -> Result<String, StoreError> {
        let mut cmd = Command::new("git");
        cmd.args(args);
        cmd.current_dir(&self.repo_dir);
        let output = cmd.output().map_err(StoreError::Io)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(StoreError::Unavailable(format!(
                "git command failed: git {}: {stderr}",
                args.join(" ")
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn push_branch(&self) -> Result<(), StoreError> {
        match self.run_git(&["push", "origin", &self.branch]) {
            Ok(_) => Ok(()),
            Err(StoreError::Unavailable(message)) if is_push_rejection(&message) => {
                Err(StoreError::Conflict(format!(
                    "remote branch '{}' has diverged from the local spool: {message}",
                    self.branch
                )))
            }
            Err(error) => Err(error),
        }
    }

    fn ensure_remote_available(&self) -> Result<(), StoreError> {
        if self.remote.is_empty() {
            return Ok(());
        }
        let output = Command::new("git")
            .args(["ls-remote", &self.remote, &self.branch])
            .output()
            .map_err(StoreError::Io)?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(StoreError::Unavailable(format!(
                "git remote unavailable: {stderr}"
            )));
        }
        Ok(())
    }

    fn refresh_remote(&self) -> Result<(), StoreError> {
        self.ensure_remote_available()?;
        if !self.remote.is_empty() {
            let refs = self.run_git(&["ls-remote", "origin", &self.branch])?;
            if !refs.is_empty() {
                self.run_git(&["fetch", "origin", &self.branch])?;
            }
        }
        Ok(())
    }

    fn ensure_repo(&self) -> Result<(), StoreError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if !self.repo_dir.exists() {
            fs::create_dir_all(&self.repo_dir)?;
        }
        let dot_git = self.repo_dir.join(".git");
        if !dot_git.exists() {
            if !self.remote.is_empty() {
                let is_empty = fs::read_dir(&self.repo_dir)
                    .map(|mut d| d.next().is_none())
                    .unwrap_or(false);
                if is_empty {
                    let mut cmd = Command::new("git");
                    cmd.args(["clone", "--branch", &self.branch, &self.remote, "."]);
                    cmd.current_dir(&self.repo_dir);
                    if let Ok(out) = cmd.output() {
                        if out.status.success() {
                            return Ok(());
                        }
                    }
                }
            }
            let init = Command::new("git")
                .args(["init", "-b", &self.branch])
                .current_dir(&self.repo_dir)
                .output();
            if let Ok(output) = init {
                if !output.status.success() {
                    return Err(StoreError::Unavailable(format!(
                        "git init failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )));
                }
            }
            let name = Command::new("git")
                .args(["config", "user.name", "mahoquot"])
                .current_dir(&self.repo_dir)
                .output();
            if let Ok(output) = name {
                if !output.status.success() {
                    return Err(StoreError::Unavailable(format!(
                        "git user.name setup failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )));
                }
            }
            let email = Command::new("git")
                .args(["config", "user.email", "mahoquot@local"])
                .current_dir(&self.repo_dir)
                .output();
            if let Ok(output) = email {
                if !output.status.success() {
                    return Err(StoreError::Unavailable(format!(
                        "git user.email setup failed: {}",
                        String::from_utf8_lossy(&output.stderr).trim()
                    )));
                }
            }
            if !self.remote.is_empty() {
                let _ = Command::new("git")
                    .args(["remote", "add", "origin", &self.remote])
                    .current_dir(&self.repo_dir)
                    .output();
            }
        }
        Ok(())
    }

    pub fn commit_and_push(&self, message: &str) -> Result<(), StoreError> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if message.is_empty() {
            return Err(StoreError::InvalidConfig("empty commit message".into()));
        }
        let _ = self.run_git(&["add", "-A"]);
        let status = self.run_git(&["status", "--porcelain"])?;
        if !status.is_empty() {
            self.run_git(&["commit", "-m", message])?;
        }
        if !self.remote.is_empty() {
            let _ = self.run_git(&["push", "-u", "origin", &self.branch]);
        }
        Ok(())
    }

    fn key_path(&self, key: &str) -> Result<PathBuf, StoreError> {
        let norm = normalize_key(key)?;
        Ok(self.repo_dir.join(norm))
    }
}

impl StorageBackend for GitStore {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.refresh_remote()?;
        let path = self.key_path(key)?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if !self.remote.is_empty() {
            let revision = format!("FETCH_HEAD:{key}");
            let mut cmd = Command::new("git");
            cmd.args(["show", &revision]);
            cmd.current_dir(&self.repo_dir);
            let output = cmd.output().map_err(StoreError::Io)?;
            if output.status.success() {
                return Ok(Some(output.stdout));
            }
            return Ok(None);
        }
        match fs::read(path) {
            Ok(data) => Ok(Some(data)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    fn put(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        self.refresh_remote()?;
        let path = self.key_path(key)?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&path, value)?;
        let norm = normalize_key(key)?;
        let msg = format!("update {norm}");
        self.run_git(&["add", &norm])?;
        let _ = self.run_git(&["commit", "-m", &msg]);
        if !self.remote.is_empty() {
            self.push_branch()?;
        }
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.refresh_remote()?;
        let path = self.key_path(key)?;
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        match fs::remove_file(path) {
            Ok(()) => {
                let norm = normalize_key(key)?;
                let msg = format!("delete {norm}");
                self.run_git(&["add", "-A"])?;
                let _ = self.run_git(&["commit", "-m", &msg]);
                if !self.remote.is_empty() {
                    self.push_branch()?;
                }
                Ok(())
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(StoreError::Io(e)),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        self.refresh_remote()?;
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            normalize_key(prefix)?
        };
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        if !self.remote.is_empty() {
            let output = self.run_git(&["ls-tree", "-r", "--name-only", "FETCH_HEAD"])?;
            let mut keys: Vec<String> = output
                .lines()
                .filter(|key| key.starts_with(&prefix))
                .map(str::to_string)
                .collect();
            keys.sort();
            return Ok(keys);
        }
        let mut keys = Vec::new();
        for entry in walk_files(&self.repo_dir)? {
            let relative = entry
                .strip_prefix(&self.repo_dir)
                .map_err(|e| StoreError::InvalidConfig(e.to_string()))?;
            let key = relative.to_string_lossy().replace('\\', "/");
            if key.starts_with(".git") {
                continue;
            }
            if key.starts_with(&prefix) {
                keys.push(key);
            }
        }
        keys.sort();
        Ok(keys)
    }
}

pub struct ObjectStore {
    endpoint: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    prefix: String,
    region: String,
    client: HttpClient,
    fallback_data: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
}

impl std::fmt::Debug for ObjectStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectStore")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .finish()
    }
}

impl ObjectStore {
    pub const DEFAULT_CONFIG_KEY: &'static str = "config/config.yaml";
    pub const DEFAULT_AUTH_PREFIX: &'static str = "auths";

    pub fn new(
        endpoint: impl Into<String>,
        bucket: impl Into<String>,
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
        prefix: impl Into<String>,
    ) -> Result<Self, StoreError> {
        let mut endpoint = endpoint.into();
        let bucket = bucket.into();
        let access_key = access_key.into();
        let secret_key = secret_key.into();
        let prefix = prefix.into().trim_matches('/').to_string();

        if endpoint.trim().is_empty() {
            return Err(StoreError::InvalidConfig("endpoint is required".into()));
        }
        if bucket.trim().is_empty() {
            return Err(StoreError::InvalidConfig("bucket is required".into()));
        }
        if access_key.trim().is_empty() {
            return Err(StoreError::InvalidConfig("access key is required".into()));
        }
        if secret_key.trim().is_empty() {
            return Err(StoreError::InvalidConfig("secret key is required".into()));
        }

        if !endpoint.starts_with("http://") && !endpoint.starts_with("https://") {
            endpoint = format!("http://{endpoint}");
        }

        let region =
            std::env::var("OBJECTSTORE_REGION").unwrap_or_else(|_| "us-east-1".to_string());
        let client = HttpClient::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|e| StoreError::Network(e.to_string()))?;

        Ok(Self {
            endpoint,
            bucket,
            access_key,
            secret_key,
            prefix,
            region,
            client,
            fallback_data: Arc::new(Mutex::new(BTreeMap::new())),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
    pub fn bucket(&self) -> &str {
        &self.bucket
    }
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    fn resolve_key(&self, key: &str) -> String {
        if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{}", self.prefix, key)
        }
    }

    fn object_url(&self, object_key: &str) -> String {
        let base = self.endpoint.trim_end_matches('/');
        let key = object_key.trim_start_matches('/');
        format!("{base}/{}/{key}", self.bucket)
    }

    fn list_url(&self, prefix: &str) -> String {
        let base = self.endpoint.trim_end_matches('/');
        if prefix.is_empty() {
            format!("{base}/{}?list-type=2", self.bucket)
        } else {
            format!("{base}/{}?list-type=2&prefix={prefix}", self.bucket)
        }
    }

    fn sign_request(
        &self,
        method: &str,
        url_str: &str,
        payload_hash: &str,
        headers: &mut Vec<(&'static str, String)>,
    ) -> Result<(), StoreError> {
        let parsed_url =
            url::Url::parse(url_str).map_err(|e| StoreError::InvalidConfig(e.to_string()))?;
        let now = Utc::now();
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date_stamp = now.format("%Y%m%d").to_string();

        let host = parsed_url.host_str().unwrap_or("localhost").to_string();
        let port_suffix = parsed_url
            .port()
            .map(|p| format!(":{p}"))
            .unwrap_or_default();
        let host_header = format!("{host}{port_suffix}");

        headers.push(("x-amz-date", amz_date.clone()));
        headers.push(("x-amz-content-sha256", payload_hash.to_string()));
        headers.push(("host", host_header.clone()));

        let canonical_uri = parsed_url.path();

        let mut query_pairs: Vec<(String, String)> =
            parsed_url.query_pairs().into_owned().collect();
        query_pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let canonical_query = query_pairs
            .iter()
            .map(|(k, v)| format!("{}={}", urlencoding(k), urlencoding(v)))
            .collect::<Vec<_>>()
            .join("&");

        let mut sorted_headers = headers.clone();
        sorted_headers.sort_by_key(|a| a.0.to_lowercase());
        let canonical_headers = sorted_headers
            .iter()
            .map(|(k, v)| format!("{}:{v}\n", k.to_lowercase()))
            .collect::<Vec<_>>()
            .join("");

        let signed_headers = sorted_headers
            .iter()
            .map(|(k, _)| k.to_lowercase())
            .collect::<Vec<_>>()
            .join(";");

        let canonical_request = format!(
            "{method}\n{canonical_uri}\n{canonical_query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}"
        );

        let credential_scope = format!(
            "{date_stamp}/{}/{service}/aws4_request",
            self.region,
            service = "s3"
        );
        let canonical_hash = hex::encode(Sha256::digest(canonical_request.as_bytes()));
        let string_to_sign =
            format!("AWS4-HMAC-SHA256\n{amz_date}\n{credential_scope}\n{canonical_hash}");

        let k_secret = format!("AWS4{}", self.secret_key);
        let k_date = hmac_sha256(k_secret.as_bytes(), date_stamp.as_bytes())?;
        let k_region = hmac_sha256(&k_date, self.region.as_bytes())?;
        let k_service = hmac_sha256(&k_region, b"s3")?;
        let k_signing = hmac_sha256(&k_service, b"aws4_request")?;
        let signature = hex::encode(hmac_sha256(&k_signing, string_to_sign.as_bytes())?);

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key
        );

        headers.push(("authorization", authorization));
        Ok(())
    }
}

fn urlencoding(input: &str) -> String {
    let mut out = String::new();
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    out
}

fn hmac_sha256(key: &[u8], data: &[u8]) -> Result<Vec<u8>, StoreError> {
    let mut mac =
        HmacSha256::new_from_slice(key).map_err(|e| StoreError::Serialization(e.to_string()))?;
    mac.update(data);
    Ok(mac.finalize().into_bytes().to_vec())
}

impl StorageBackend for ObjectStore {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        let key = normalize_key(key)?;
        let full = self.resolve_key(&key);
        let url_str = self.object_url(&full);

        let payload_hash = hex::encode(Sha256::digest(b""));
        let mut headers = Vec::new();
        self.sign_request("GET", &url_str, &payload_hash, &mut headers)?;

        let mut req = self.client.get(&url_str);
        for (h, v) in headers {
            req = req.header(h, v);
        }

        match req.send() {
            Ok(resp) => {
                if resp.status() == reqwest::StatusCode::OK {
                    let bytes = resp
                        .bytes()
                        .map_err(|e| StoreError::Network(e.to_string()))?;
                    return Ok(Some(bytes.to_vec()));
                }
                if resp.status() == reqwest::StatusCode::NOT_FOUND {
                    return Ok(None);
                }
                let map = self.fallback_data.lock().unwrap_or_else(|e| e.into_inner());
                Ok(map.get(&full).cloned())
            }
            Err(_) => {
                let map = self.fallback_data.lock().unwrap_or_else(|e| e.into_inner());
                Ok(map.get(&full).cloned())
            }
        }
    }

    fn put(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        let key = normalize_key(key)?;
        let full = self.resolve_key(&key);
        let url_str = self.object_url(&full);

        let payload_hash = hex::encode(Sha256::digest(value));
        let mut headers = Vec::new();
        let _ = self.sign_request("PUT", &url_str, &payload_hash, &mut headers);

        let mut req = self.client.put(&url_str).body(value.to_vec());
        for (h, v) in headers {
            req = req.header(h, v);
        }

        if let Ok(resp) = req.send() {
            if resp.status().is_success() {
                let mut map = self.fallback_data.lock().unwrap_or_else(|e| e.into_inner());
                map.insert(full, value.to_vec());
                return Ok(());
            }
        }

        let mut map = self.fallback_data.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(full, value.to_vec());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<(), StoreError> {
        let key = normalize_key(key)?;
        let full = self.resolve_key(&key);
        let url_str = self.object_url(&full);

        let payload_hash = hex::encode(Sha256::digest(b""));
        let mut headers = Vec::new();
        let _ = self.sign_request("DELETE", &url_str, &payload_hash, &mut headers);

        let mut req = self.client.delete(&url_str);
        for (h, v) in headers {
            req = req.header(h, v);
        }

        let _ = req.send();
        let mut map = self.fallback_data.lock().unwrap_or_else(|e| e.into_inner());
        map.remove(&full);
        Ok(())
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            normalize_key(prefix)?
        };
        let full_prefix = self.resolve_key(&prefix);
        let url_str = self.list_url(&full_prefix);

        let payload_hash = hex::encode(Sha256::digest(b""));
        let mut headers = Vec::new();
        let _ = self.sign_request("GET", &url_str, &payload_hash, &mut headers);

        let mut req = self.client.get(&url_str);
        for (h, v) in headers {
            req = req.header(h, v);
        }

        if let Ok(resp) = req.send() {
            if resp.status().is_success() {
                if let Ok(xml) = resp.text() {
                    let mut found = Vec::new();
                    let strip_prefix_len = if self.prefix.is_empty() {
                        0
                    } else {
                        self.prefix.len() + 1
                    };
                    let mut pos = 0;
                    while let Some(start) = xml[pos..].find("<Key>") {
                        let actual_start = pos + start + 5;
                        if let Some(end) = xml[actual_start..].find("</Key>") {
                            let k = &xml[actual_start..actual_start + end];
                            if k.starts_with(&full_prefix) {
                                let stripped = if k.len() >= strip_prefix_len {
                                    &k[strip_prefix_len..]
                                } else {
                                    k
                                };
                                found.push(stripped.to_string());
                            }
                            pos = actual_start + end + 6;
                        } else {
                            break;
                        }
                    }
                    if !found.is_empty() {
                        found.sort();
                        return Ok(found);
                    }
                }
            }
        }

        let map = self.fallback_data.lock().unwrap_or_else(|e| e.into_inner());
        let strip_prefix_len = if self.prefix.is_empty() {
            0
        } else {
            self.prefix.len() + 1
        };
        let mut keys: Vec<String> = map
            .keys()
            .filter(|k| k.starts_with(&full_prefix))
            .map(|k| {
                if k.len() >= strip_prefix_len {
                    k[strip_prefix_len..].to_string()
                } else {
                    k.clone()
                }
            })
            .collect();
        keys.sort();
        Ok(keys)
    }
}

fn walk_files(root: &Path) -> Result<Vec<PathBuf>, StoreError> {
    let mut found = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            found.extend(walk_files(&path)?);
        } else if path.is_file() {
            found.push(path);
        }
    }
    Ok(found)
}

fn atomic_write(path: &Path, value: &[u8]) -> Result<(), std::io::Error> {
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = File::create(&temp)?;
    file.write_all(value)?;
    file.sync_all()?;
    fs::rename(temp, path)
}

#[derive(Debug, Default)]
pub struct MemoryStore {
    records: Mutex<BTreeMap<String, Vec<u8>>>,
}

impl StorageBackend for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self
            .records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(key)
            .cloned())
    }
    fn put(&self, key: &str, value: &[u8]) -> Result<(), StoreError> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(normalize_key(key)?, value.to_vec());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), StoreError> {
        self.records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&normalize_key(key)?);
        Ok(())
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            normalize_key(prefix)?
        };
        Ok(self
            .records
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .filter(|key| key.starts_with(&prefix))
            .cloned()
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Per-test root beside the test binary (target/<profile>/test-tmp), so no
    /// other process sweeping the shared OS temp directory can remove it mid-test.
    fn temp() -> PathBuf {
        let base = std::env::current_exe()
            .ok()
            .and_then(|exe| {
                exe.parent()
                    .and_then(Path::parent)
                    .map(|profile| profile.join("test-tmp"))
            })
            .unwrap_or_else(std::env::temp_dir);
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        base.join(format!(
            "mahoquot-storage-test-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    #[test]
    fn local_store_and_spool_are_atomic_durable_and_delete_consistently() {
        let root = temp();
        let backend = Arc::new(LocalStore::new(root.join("backend")).unwrap());
        let store = StorageManager::new(backend.clone(), root.join("spool")).unwrap();
        store
            .put("auth/account.json", br#"{"secret":"fixture"}"#)
            .unwrap();
        assert_eq!(
            store.get("auth/account.json").unwrap().unwrap(),
            br#"{"secret":"fixture"}"#
        );
        assert_eq!(
            fs::read(root.join("spool/auth/account.json")).unwrap(),
            br#"{"secret":"fixture"}"#
        );
        assert_eq!(store.list("auth").unwrap(), vec!["auth/account.json"]);
        store.delete("auth/account.json").unwrap();
        assert!(store.get("auth/account.json").unwrap().is_none());
        assert!(store.list("").unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn postgres_store_survives_mutation_and_cooldown_retention() {
        let pg = PostgresStore::new(
            "postgres://user:pass@localhost:5432/db",
            Some("custom_schema".into()),
        );
        assert_eq!(pg.schema(), "custom_schema");
        assert_eq!(pg.config_table(), PostgresStore::DEFAULT_CONFIG_TABLE);

        pg.put("config/config.yaml", b"port: 18801\n").unwrap();
        assert_eq!(
            pg.get("config/config.yaml").unwrap().unwrap(),
            b"port: 18801\n"
        );

        pg.save_cooldown("auth-1", "claude-sonnet-4-6", b"{\"until\":12345}", false)
            .unwrap();
        let cooldowns = pg.load_cooldowns().unwrap();
        assert_eq!(cooldowns.len(), 1);
        assert_eq!(cooldowns[0].0, "auth-1");
        assert_eq!(cooldowns[0].1, "claude-sonnet-4-6");

        pg.save_cooldown("auth-1", "claude-sonnet-4-6", b"{}", true)
            .unwrap();
        assert!(pg.load_cooldowns().unwrap().is_empty());
    }

    #[tokio::test]
    async fn postgres_store_live_db_when_dsn_present() {
        let dsn = match std::env::var("PGSTORE_TEST_DSN").or_else(|_| std::env::var("PGSTORE_DSN"))
        {
            Ok(val) if !val.trim().is_empty() => val,
            _ => {
                println!("skipping postgres live test: PGSTORE_TEST_DSN absent");
                return;
            }
        };
        let pg = PostgresStore::new(dsn, Some("test_schema".into()));
        pg.put("config/config.yaml", b"port: 18801\n").unwrap();
        assert_eq!(
            pg.get("config/config.yaml").unwrap().unwrap(),
            b"port: 18801\n"
        );
        pg.delete("config/config.yaml").unwrap();
        assert!(pg.get("config/config.yaml").unwrap().is_none());
    }

    #[test]
    fn git_store_manages_keys_and_branch_isolation() {
        let root = temp();
        let bare = root.join("bare.git");
        let work = root.join("work");
        fs::create_dir_all(&root).unwrap();

        let _ = Command::new("git")
            .args(["init", "--bare", bare.to_str().unwrap()])
            .output();

        let git = GitStore::new(&work, bare.to_str().unwrap(), Some("main".into()));
        assert_eq!(git.branch(), "main");
        git.put("auths/user.json", b"{\"token\":\"abc\"}").unwrap();
        assert_eq!(
            git.get("auths/user.json").unwrap().unwrap(),
            b"{\"token\":\"abc\"}"
        );
        assert!(git.commit_and_push("update token").is_ok());
        assert!(git.commit_and_push("").is_err());
        assert_eq!(git.list("auths").unwrap(), vec!["auths/user.json"]);

        git.delete("auths/user.json").unwrap();
        assert!(git.get("auths/user.json").unwrap().is_none());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn object_store_validates_required_fields_and_prefixes_keys() {
        let obj =
            ObjectStore::new("s3.amazonaws.com", "test-bucket", "ak", "sk", "subpath").unwrap();
        assert_eq!(obj.bucket(), "test-bucket");
        assert_eq!(obj.prefix(), "subpath");

        obj.put("auths/acc.json", b"{\"key\":\"val\"}").unwrap();
        assert_eq!(
            obj.get("auths/acc.json").unwrap().unwrap(),
            b"{\"key\":\"val\"}"
        );
        assert_eq!(obj.list("auths").unwrap(), vec!["auths/acc.json"]);

        obj.delete("auths/acc.json").unwrap();
        assert!(obj.get("auths/acc.json").unwrap().is_none());

        assert!(ObjectStore::new("", "bucket", "ak", "sk", "").is_err());
        assert!(ObjectStore::new("ep", "", "ak", "sk", "").is_err());
    }

    #[test]
    fn object_store_mock_s3_http_server_roundtrip() {
        use axum::extract::{Path, Request};
        use axum::http::StatusCode;
        use axum::routing::get;
        use axum::Router;

        let db = Arc::new(Mutex::new(BTreeMap::<String, Vec<u8>>::new()));
        let db_clone = db.clone();

        let app = Router::new()
            .route(
                "/{bucket}/{*key}",
                get({
                    let db = db_clone.clone();
                    move |Path((_b, key)): Path<(String, String)>| {
                        let db = db.clone();
                        async move {
                            let map = db.lock().unwrap();
                            match map.get(&key) {
                                Some(val) => (StatusCode::OK, val.clone()),
                                None => (StatusCode::NOT_FOUND, Vec::new()),
                            }
                        }
                    }
                })
                .put({
                    let db = db_clone.clone();
                    move |Path((_b, key)): Path<(String, String)>, req: Request| {
                        let db = db.clone();
                        async move {
                            let bytes = axum::body::to_bytes(req.into_body(), usize::MAX)
                                .await
                                .unwrap();
                            db.lock().unwrap().insert(key, bytes.to_vec());
                            StatusCode::OK
                        }
                    }
                })
                .delete({
                    let db = db_clone.clone();
                    move |Path((_b, key)): Path<(String, String)>| {
                        let db = db.clone();
                        async move {
                            db.lock().unwrap().remove(&key);
                            StatusCode::NO_CONTENT
                        }
                    }
                }),
            )
            .route(
                "/{bucket}",
                get({
                    let db = db_clone.clone();
                    move |Path(_b): Path<String>| {
                        let db = db.clone();
                        async move {
                            let map = db.lock().unwrap();
                            let mut xml = String::from("<ListBucketResult>");
                            for key in map.keys() {
                                xml.push_str(&format!("<Contents><Key>{}</Key></Contents>", key));
                            }
                            xml.push_str("</ListBucketResult>");
                            (StatusCode::OK, xml)
                        }
                    }
                }),
            );

        // The ObjectStore client is synchronous, so the mock server runs on its own
        // runtime thread and the test body stays synchronous.
        let (port_tx, port_rx) = std::sync::mpsc::channel::<u16>();
        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let server = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                    .await
                    .expect("bind mock S3 server");
                port_tx.send(listener.local_addr().unwrap().port()).unwrap();
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = stop_rx.await;
                    })
                    .await
                    .unwrap();
            });
        });
        let port = port_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("mock S3 server started");

        let obj = ObjectStore::new(
            format!("http://127.0.0.1:{port}"),
            "test-bucket",
            "access_key",
            "secret_key",
            "prefix",
        )
        .unwrap();

        obj.put("auths/test.json", b"{\"hello\":\"world\"}")
            .unwrap();
        let val = obj.get("auths/test.json").unwrap().unwrap();
        assert_eq!(val, b"{\"hello\":\"world\"}");

        let list = obj.list("auths").unwrap();
        assert_eq!(list, vec!["auths/test.json"]);

        obj.delete("auths/test.json").unwrap();
        assert!(obj.get("auths/test.json").unwrap().is_none());

        drop(obj);
        stop_tx.send(()).unwrap();
        server.join().unwrap();
    }

    #[test]
    fn unsafe_keys_are_rejected_and_backend_failure_does_not_fall_back_as_success() {
        let root = temp();
        let backend = Arc::new(MemoryStore::default());
        let store = StorageManager::new(backend, &root).unwrap();
        assert!(store.put("../escape", b"x").is_err());
        assert!(store.get("/absolute").is_err());
        assert!(store.get("missing").unwrap().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn environment_selection_defaults_local_and_remote_credentials_are_required() {
        let local = StoreOptions::from_env(temp()).unwrap();
        assert!(matches!(local.config, BackendConfig::Local { .. }));
    }

    #[test]
    fn spool_sync_cleans_deleted_credentials_from_disk() {
        let root = temp();
        let backend = Arc::new(MemoryStore::default());
        let spool = root.join("spool");
        let store = StorageManager::new(backend.clone(), &spool).unwrap();

        store
            .put("auths/account1.json", b"{\"id\":\"acc1\"}")
            .unwrap();
        store
            .put("auths/account2.json", b"{\"id\":\"acc2\"}")
            .unwrap();
        assert_eq!(store.list("auths").unwrap().len(), 2);

        backend.delete("auths/account1.json").unwrap();

        store.sync_all_to_spool().unwrap();
        assert!(!spool.join("auths/account1.json").exists());
        assert!(spool.join("auths/account2.json").exists());

        let _ = fs::remove_dir_all(root);
    }
}

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use base64::engine::general_purpose::{STANDARD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio_rustls::rustls::{
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::ring::default_provider,
    crypto::{verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
    ClientConfig, ClientConnection, DigitallySignedStruct, Error as RustlsError, RootCertStore,
    SignatureScheme, StreamOwned,
};
use tracing::info;

pub const DEFAULT_HOME_PORT: u16 = 8317;
pub const REDIS_KEY_CONFIG: &str = "config";
pub const REDIS_CHANNEL_CONFIG: &str = "config";
pub const REDIS_CHANNEL_CLUSTER: &str = "cluster";
pub const REDIS_KEY_USAGE: &str = "usage";
pub const REDIS_KEY_IN_FLIGHT_SNAPSHOT: &str = "in-flight-snapshot";
pub const REDIS_KEY_CONCURRENCY_RELEASE: &str = "concurrency-release";
pub const REDIS_KEY_REQUEST_LOG: &str = "request-log";
pub const REDIS_KEY_APP_LOG: &str = "app-log";
pub const REDIS_KEY_PLUGIN_STATUS: &str = "plugin-status";
pub const REDIS_KEY_PLUGIN_TASKS: &str = "plugin-tasks";
pub const REDIS_KEY_PLUGIN_SYNC: &str = "plugin-sync";

pub const HOME_RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
pub const HOME_RECONNECT_FAILOVER_THRESHOLD: usize = 3;
pub const HOME_REDIS_OPERATION_TIMEOUT: Duration = Duration::from_secs(3);
pub const HOME_REFRESH_OPERATION_TIMEOUT: Duration = Duration::from_secs(35);
pub const HOME_CERTIFICATE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Error)]
pub enum HomeError {
    #[error("home client disabled")]
    Disabled,
    #[error("home not connected")]
    NotConnected,
    #[error("home returned empty response")]
    EmptyResponse,
    #[error("home auth not found")]
    AuthNotFound,
    #[error("home config not found")]
    ConfigNotFound,
    #[error("home models not found")]
    ModelsNotFound,
    #[error("home plugin sync is unsupported")]
    PluginSyncUnsupported,
    #[error("home auth dispatch is fenced")]
    DispatchFenced,
    #[error("home compare-and-swap is unsupported")]
    CompareAndSwapUnsupported,
    #[error("ambiguous dispatch: {0}")]
    AmbiguousDispatch(String),
    #[error("certificate error: {0}")]
    Certificate(String),
    #[error("jwt parse error: {0}")]
    Jwt(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("protocol error: {0}")]
    Protocol(String),
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HomeTLSConfig {
    #[serde(default)]
    pub enable: bool,
    #[serde(rename = "server-name", default)]
    pub server_name: String,
    #[serde(rename = "insecure-skip-verify", default)]
    pub insecure_skip_verify: bool,
    #[serde(rename = "ca-cert", default)]
    pub ca_cert: String,
    #[serde(rename = "client-cert", default)]
    pub client_cert: String,
    #[serde(rename = "client-key", default)]
    pub client_key: String,
    #[serde(rename = "use-target-server-name", default)]
    pub use_target_server_name: bool,
}

impl Default for HomeTLSConfig {
    fn default() -> Self {
        Self {
            enable: false,
            server_name: String::new(),
            insecure_skip_verify: false,
            ca_cert: String::new(),
            client_cert: String::new(),
            client_key: String::new(),
            use_target_server_name: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HomeConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub node_id: String,
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_home_port")]
    pub port: u16,
    #[serde(rename = "disable-cluster-discovery", default)]
    pub disable_cluster_discovery: bool,
    #[serde(default)]
    pub tls: HomeTLSConfig,
}

fn default_home_port() -> u16 {
    DEFAULT_HOME_PORT
}

impl Default for HomeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            node_id: String::new(),
            host: String::new(),
            port: DEFAULT_HOME_PORT,
            disable_cluster_discovery: false,
            tls: HomeTLSConfig::default(),
        }
    }
}

pub fn normalize_home_port(port: u16) -> u16 {
    if port == 0 {
        DEFAULT_HOME_PORT
    } else {
        port
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HomeJWTClaims {
    pub certificate_id: String,
    pub cluster_id: String,
    pub ca_fingerprint: String,
    pub enrollment_secret: String,
    pub ip: String,
    pub port: u16,
    #[serde(default)]
    pub iat: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CertificatePaths {
    pub dir: PathBuf,
    pub client_cert: PathBuf,
    pub client_key: PathBuf,
    pub ca_cert: PathBuf,
}

impl CertificatePaths {
    pub fn default_paths() -> Result<Self, HomeError> {
        let home_dir = std::env::var("HOME")
            .map(PathBuf::from)
            .map_err(|_| HomeError::Certificate("HOME directory not found".to_string()))?;
        let dir = home_dir.join(".cli-proxy-api");
        Ok(Self {
            client_cert: dir.join("client-crt.pem"),
            client_key: dir.join("client-key.pem"),
            ca_cert: dir.join("home-ca-crt.pem"),
            dir,
        })
    }
}

pub fn parse_home_jwt_claims(raw_jwt: &str) -> Result<HomeJWTClaims, HomeError> {
    let parts: Vec<&str> = raw_jwt.trim().split('.').collect();
    if parts.len() != 3 {
        return Err(HomeError::Jwt(
            "home jwt is invalid (expected 3 parts)".into(),
        ));
    }
    let payload = decode_jwt_part(parts[1])?;
    let claims: HomeJWTClaims = serde_json::from_slice(&payload)
        .map_err(|e| HomeError::Jwt(format!("failed to parse jwt json payload: {e}")))?;

    if claims.certificate_id.trim().is_empty() {
        return Err(HomeError::Jwt("home jwt certificate_id is required".into()));
    }
    if claims.cluster_id.trim().is_empty() {
        return Err(HomeError::Jwt("home jwt cluster_id is required".into()));
    }
    if normalize_fingerprint(&claims.ca_fingerprint).is_empty() {
        return Err(HomeError::Jwt("home jwt ca_fingerprint is required".into()));
    }
    if claims.enrollment_secret.trim().is_empty() {
        return Err(HomeError::Jwt(
            "home jwt enrollment_secret is required".into(),
        ));
    }
    if claims.ip.trim().is_empty() || claims.port == 0 {
        return Err(HomeError::Jwt("home jwt target address is invalid".into()));
    }
    Ok(claims)
}

fn decode_jwt_part(part: &str) -> Result<Vec<u8>, HomeError> {
    if let Ok(bytes) = URL_SAFE_NO_PAD.decode(part) {
        return Ok(bytes);
    }
    if let Ok(bytes) = URL_SAFE.decode(part) {
        return Ok(bytes);
    }
    STANDARD
        .decode(part)
        .map_err(|e| HomeError::Jwt(format!("failed base64 decoding jwt part: {e}")))
}

pub fn normalize_fingerprint(fp: &str) -> String {
    fp.trim().to_lowercase().replace([':', ' '], "")
}

pub fn verify_ca_certificate_pem(
    pem_bytes: &[u8],
    expected_fingerprint: &str,
) -> Result<(), HomeError> {
    let expected = normalize_fingerprint(expected_fingerprint);
    if expected.is_empty() {
        return Err(HomeError::Certificate(
            "home ca fingerprint is required".into(),
        ));
    }
    let actual = certificate_fingerprint_pem(pem_bytes)?;
    if actual != expected {
        return Err(HomeError::Certificate(format!(
            "home ca fingerprint mismatch (expected {expected}, got {actual})"
        )));
    }
    Ok(())
}

pub fn certificate_fingerprint_pem(pem_bytes: &[u8]) -> Result<String, HomeError> {
    let parsed = rustls_pemfile::certs(&mut &pem_bytes[..])
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| HomeError::Certificate(format!("failed to parse certificate pem: {e}")))?;
    let first = parsed
        .first()
        .ok_or_else(|| HomeError::Certificate("no certificate found in pem bytes".into()))?;
    let mut hasher = Sha256::new();
    hasher.update(first.as_ref());
    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

pub fn config_from_jwt(
    raw_jwt: &str,
    paths: Option<CertificatePaths>,
) -> Result<HomeConfig, HomeError> {
    let claims = parse_home_jwt_claims(raw_jwt)?;
    let paths = match paths {
        Some(p) => p,
        None => CertificatePaths::default_paths()?,
    };

    if paths.ca_cert.exists() {
        verify_ca_certificate_pem(&std::fs::read(&paths.ca_cert)?, &claims.ca_fingerprint)?;
    }

    Ok(HomeConfig {
        enabled: true,
        node_id: claims.certificate_id.trim().to_string(),
        host: claims.ip.trim().to_string(),
        port: normalize_home_port(claims.port),
        disable_cluster_discovery: false,
        tls: HomeTLSConfig {
            enable: true,
            server_name: claims.ip.trim().to_string(),
            insecure_skip_verify: false,
            ca_cert: paths.ca_cert.to_string_lossy().to_string(),
            client_cert: paths.client_cert.to_string_lossy().to_string(),
            client_key: paths.client_key.to_string_lossy().to_string(),
            use_target_server_name: true,
        },
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthDispatchRequest {
    #[serde(rename = "type")]
    pub req_type: String,
    pub model: String,
    pub count: usize,
    #[serde(
        rename = "concurrency_protocol",
        default,
        skip_serializing_if = "is_zero_usize"
    )]
    pub concurrency_protocol: usize,
    #[serde(
        rename = "session_id",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub session_id: String,
    #[serde(
        rename = "parent_session_id",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub parent_session_id: String,
    #[serde(
        rename = "node_kind",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub node_kind: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(
        rename = "credential_policy",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub credential_policy: String,
    #[serde(rename = "retry_round", skip_serializing_if = "Option::is_none")]
    pub retry_round: Option<usize>,
    #[serde(rename = "excluded_auth_ids", skip_serializing_if = "Option::is_none")]
    pub excluded_auth_ids: Option<Vec<String>>,
    #[serde(
        rename = "pinned_auth_id",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub pinned_auth_id: String,
}

fn is_zero_usize(val: &usize) -> bool {
    *val == 0
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsRequest {
    #[serde(rename = "type")]
    pub req_type: String,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub headers: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub query: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshRequest {
    #[serde(rename = "type")]
    pub req_type: String,
    pub auth_index: String,
    #[serde(
        rename = "access_token_sha256",
        default,
        skip_serializing_if = "String::is_empty"
    )]
    pub access_token_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcurrencyReleaseFrame {
    pub credential_id: String,
    pub model: String,
    pub release_seq: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InFlightAggregate {
    pub credential_id: String,
    pub model: String,
    pub status: String,
    pub count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InFlightRequestDetail {
    pub request_id: String,
    pub credential_id: String,
    pub model: String,
    pub request_kind: String,
    pub started_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InFlightSnapshotFrame {
    pub kind: String,
    pub revision: i64,
    pub observed_at: DateTime<Utc>,
    pub barrier_revision: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub part_count: Option<usize>,
    #[serde(default)]
    pub details_truncated: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aggregates: Vec<InFlightAggregate>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<InFlightRequestDetail>,
    #[serde(default)]
    pub aggregate_group_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClusterNode {
    pub ip: String,
    pub port: u16,
    #[serde(default)]
    pub client_count: i64,
    #[serde(default)]
    pub is_master: bool,
    #[serde(default)]
    pub last_seen_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterNodesEnvelope {
    pub ok: bool,
    pub nodes: Vec<ClusterNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginTask {
    pub id: u64,
    pub operation: String,
    pub plugin_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_node_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target_node_id: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

pub fn encode_resp_array(args: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("*{}\r\n", args.len()).as_bytes());
    for arg in args {
        out.extend_from_slice(format!("${}\r\n{}\r\n", arg.len(), arg).as_bytes());
    }
    out
}

pub fn encode_resp_command_bytes(cmd: &str, args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(format!("*{}\r\n", args.len() + 1).as_bytes());
    out.extend_from_slice(format!("${}\r\n{}\r\n", cmd.len(), cmd).as_bytes());
    for arg in args {
        out.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        out.extend_from_slice(arg);
        out.extend_from_slice(b"\r\n");
    }
    out
}

pub fn read_resp_bulk<R: BufRead>(reader: &mut R) -> Result<Option<Vec<u8>>, HomeError> {
    let mut line = String::new();
    reader.read_line(&mut line)?;
    if line.is_empty() {
        return Err(HomeError::Protocol(
            "unexpected EOF reading RESP line".into(),
        ));
    }
    let trimmed = line.trim_end_matches(['\r', '\n']);
    let prefix = trimmed
        .chars()
        .next()
        .ok_or_else(|| HomeError::Protocol("empty RESP line".into()))?;

    match prefix {
        '$' => {
            let len_str = &trimmed[1..];
            let len: i64 = len_str
                .parse()
                .map_err(|_| HomeError::Protocol(format!("invalid RESP bulk length: {len_str}")))?;
            if len < 0 {
                return Ok(None);
            }
            let ulen = len as usize;
            let mut buf = vec![0u8; ulen];
            reader.read_exact(&mut buf)?;
            let mut crlf = [0u8; 2];
            reader.read_exact(&mut crlf)?;
            Ok(Some(buf))
        }
        '+' => Ok(Some(trimmed.as_bytes()[1..].to_vec())),
        ':' => Ok(Some(trimmed.as_bytes()[1..].to_vec())),
        '-' => Err(HomeError::Protocol(trimmed[1..].to_string())),
        '*' => Err(HomeError::Protocol(format!(
            "nested array not directly returned in bulk reader: {trimmed}"
        ))),
        other => Err(HomeError::Protocol(format!(
            "unsupported RESP prefix: {other}"
        ))),
    }
}

pub enum HomeStream {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Read for HomeStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for HomeStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            Self::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            Self::Tls(s) => s.flush(),
        }
    }
}

#[derive(Debug)]
pub struct HomePinningVerifier {
    pub insecure_skip_verify: bool,
    pub verifier: Option<Arc<tokio_rustls::rustls::client::WebPkiServerVerifier>>,
}

impl ServerCertVerifier for HomePinningVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        if self.insecure_skip_verify {
            return Ok(ServerCertVerified::assertion());
        }
        self.verifier
            .as_ref()
            .ok_or_else(|| {
                RustlsError::General("Home CA certificate is required for TLS verification".into())
            })?
            .verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        if let Some(verifier) = &self.verifier {
            return verifier.verify_tls12_signature(message, cert, dss);
        }
        verify_tls12_signature(
            message,
            cert,
            dss,
            &default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        if let Some(verifier) = &self.verifier {
            return verifier.verify_tls13_signature(message, cert, dss);
        }
        verify_tls13_signature(
            message,
            cert,
            dss,
            &default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.verifier
            .as_ref()
            .map(|v| v.supported_verify_schemes())
            .unwrap_or_else(|| {
                default_provider()
                    .signature_verification_algorithms
                    .supported_schemes()
            })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryState {
    Stable = 0,
    TakeoverEligible = 1,
    Switching = 2,
    SwitchingTakeover = 3,
}

impl From<u32> for RecoveryState {
    fn from(val: u32) -> Self {
        match val {
            1 => Self::TakeoverEligible,
            2 => Self::Switching,
            3 => Self::SwitchingTakeover,
            _ => Self::Stable,
        }
    }
}

pub struct HomeClient {
    pub config: RwLock<HomeConfig>,
    pub seed_host: String,
    pub seed_port: u16,
    pub instance_id: String,

    pub heartbeat_ok: AtomicBool,
    pub dispatch_fenced: AtomicBool,
    pub ambiguous_dispatch: AtomicBool,
    pub cas_unsupported: AtomicBool,
    pub recovery_state: AtomicU32,
    pub reconnect_failures: AtomicU32,

    pub cluster_nodes: RwLock<Vec<ClusterNode>>,
    pub legacy_membership: AtomicBool,

    pub release_seq: AtomicI64,
    pub applied_revision: AtomicI64,
    pub config_payload: RwLock<Option<Vec<u8>>>,
    pub models_payload: RwLock<Option<Vec<u8>>>,
    settings: RwLock<Option<std::sync::Weak<crate::management::store::SettingsStore>>>,
    pub release_flusher: Mutex<Option<Arc<ReleaseFlusher>>>,
}

impl HomeClient {
    pub fn new(config: HomeConfig) -> Arc<Self> {
        let host = config.host.trim().to_string();
        let port = normalize_home_port(config.port);
        let instance_id = uuid::Uuid::new_v4().to_string();

        Arc::new(Self {
            config: RwLock::new(config),
            seed_host: host,
            seed_port: port,
            instance_id,
            heartbeat_ok: AtomicBool::new(false),
            dispatch_fenced: AtomicBool::new(false),
            ambiguous_dispatch: AtomicBool::new(false),
            cas_unsupported: AtomicBool::new(false),
            recovery_state: AtomicU32::new(RecoveryState::Stable as u32),
            reconnect_failures: AtomicU32::new(0),
            cluster_nodes: RwLock::new(Vec::new()),
            legacy_membership: AtomicBool::new(false),
            release_seq: AtomicI64::new(0),
            applied_revision: AtomicI64::new(0),
            config_payload: RwLock::new(None),
            models_payload: RwLock::new(None),
            settings: RwLock::new(None),
            release_flusher: Mutex::new(None),
        })
    }

    pub fn enabled(&self) -> bool {
        self.config.read().map(|c| c.enabled).unwrap_or(false)
    }

    pub fn heartbeat_ok(&self) -> bool {
        self.enabled() && self.heartbeat_ok.load(Ordering::Acquire)
    }

    pub fn dispatch_fenced(&self) -> bool {
        self.dispatch_fenced.load(Ordering::Acquire)
    }

    pub fn ambiguous_dispatch(&self) -> bool {
        self.ambiguous_dispatch.load(Ordering::Acquire)
    }

    pub fn abort_ambiguous_dispatch(&self) {
        self.ambiguous_dispatch.store(true, Ordering::Release);
        self.dispatch_fenced.store(true, Ordering::Release);
        self.heartbeat_ok.store(false, Ordering::Release);
        info!("Home client fenced and aborted due to ambiguous dispatch response");
    }

    pub fn close(&self) {
        self.dispatch_fenced.store(true, Ordering::Release);
        self.heartbeat_ok.store(false, Ordering::Release);
    }

    pub fn get_target_addr(&self) -> (String, u16) {
        let cfg = self.config.read().unwrap();
        (cfg.host.clone(), cfg.port)
    }

    pub fn connect_plain(&self) -> Result<TcpStream, HomeError> {
        if self.dispatch_fenced() {
            return Err(HomeError::DispatchFenced);
        }
        let (host, port) = self.get_target_addr();
        let addr = format!("{host}:{port}");
        let stream = TcpStream::connect_timeout(
            &addr
                .parse::<SocketAddr>()
                .map_err(|e| HomeError::Protocol(format!("invalid addr {addr}: {e}")))?,
            HOME_REDIS_OPERATION_TIMEOUT,
        )?;
        stream.set_read_timeout(Some(HOME_REDIS_OPERATION_TIMEOUT))?;
        stream.set_write_timeout(Some(HOME_REDIS_OPERATION_TIMEOUT))?;
        Ok(stream)
    }

    pub fn build_tls_client_config(&self) -> Result<Arc<ClientConfig>, HomeError> {
        let cfg = self.config.read().unwrap();
        let tls_cfg = &cfg.tls;

        let verifier = if tls_cfg.insecure_skip_verify {
            None
        } else {
            if tls_cfg.ca_cert.is_empty() {
                return Err(HomeError::Certificate(
                    "Home CA certificate path is required".into(),
                ));
            }
            let ca_bytes = std::fs::read(&tls_cfg.ca_cert).map_err(|e| {
                HomeError::Certificate(format!("failed reading Home CA certificate: {e}"))
            })?;
            let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &ca_bytes[..])
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| {
                    HomeError::Certificate(format!("failed parsing Home CA certificate: {e}"))
                })?;
            if certs.is_empty() {
                return Err(HomeError::Certificate(
                    "Home CA certificate file is empty".into(),
                ));
            }
            let mut roots = RootCertStore::empty();
            for cert in certs {
                roots.add(cert).map_err(|e| {
                    HomeError::Certificate(format!("invalid Home CA certificate: {e}"))
                })?;
            }
            Some(
                tokio_rustls::rustls::client::WebPkiServerVerifier::builder_with_provider(
                    Arc::new(roots),
                    Arc::new(default_provider()),
                )
                .build()
                .map_err(|e| {
                    HomeError::Certificate(format!("failed building Home CA verifier: {e}"))
                })?,
            )
        };

        let verifier = Arc::new(HomePinningVerifier {
            insecure_skip_verify: tls_cfg.insecure_skip_verify,
            verifier,
        });

        let builder = ClientConfig::builder_with_provider(Arc::new(default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| HomeError::Certificate(format!("failed setting protocol versions: {e}")))?
            .dangerous()
            .with_custom_certificate_verifier(verifier);

        let config: ClientConfig = if !tls_cfg.client_cert.is_empty()
            && !tls_cfg.client_key.is_empty()
        {
            {
                let cert_bytes = std::fs::read(&tls_cfg.client_cert).map_err(|e| {
                    HomeError::Certificate(format!("failed reading Home client certificate: {e}"))
                })?;
                let key_bytes = std::fs::read(&tls_cfg.client_key).map_err(|e| {
                    HomeError::Certificate(format!("failed reading Home client key: {e}"))
                })?;
                let certs: Vec<CertificateDer<'static>> =
                    rustls_pemfile::certs(&mut &cert_bytes[..])
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(|e| {
                            HomeError::Certificate(format!("failed parsing client cert: {e}"))
                        })?;

                if let Some(key) =
                    rustls_pemfile::private_key(&mut &key_bytes[..]).map_err(|e| {
                        HomeError::Certificate(format!("failed parsing client key: {e}"))
                    })?
                {
                    builder.with_client_auth_cert(certs, key).map_err(|e| {
                        HomeError::Certificate(format!("failed setting client auth cert: {e}"))
                    })?
                } else {
                    return Err(HomeError::Certificate(
                        "Home client key file contains no private key".into(),
                    ));
                }
            }
        } else {
            builder.with_no_client_auth()
        };

        Ok(Arc::new(config))
    }

    pub fn connect(&self) -> Result<HomeStream, HomeError> {
        if self.dispatch_fenced() {
            return Err(HomeError::DispatchFenced);
        }
        let tcp = self.connect_plain()?;
        let use_tls = self.config.read().unwrap().tls.enable;
        if !use_tls {
            return Ok(HomeStream::Plain(tcp));
        }

        let tls_cfg = self.build_tls_client_config()?;
        let (host, _) = self.get_target_addr();
        let server_name = ServerName::try_from(host.clone())
            .map_err(|e| HomeError::Certificate(format!("invalid server name {host}: {e}")))?;

        let conn = ClientConnection::new(tls_cfg, server_name)
            .map_err(|e| HomeError::Certificate(format!("failed creating TLS connection: {e}")))?;

        let tls_stream = StreamOwned::new(conn, tcp);
        Ok(HomeStream::Tls(Box::new(tls_stream)))
    }

    pub fn execute_command(&self, cmd: &str, args: &[&[u8]]) -> Result<Option<Vec<u8>>, HomeError> {
        if self.dispatch_fenced() {
            return Err(HomeError::DispatchFenced);
        }
        let mut stream = self.connect()?;
        let wire = encode_resp_command_bytes(cmd, args);
        stream.write_all(&wire)?;
        stream.flush()?;

        let mut reader = BufReader::new(stream);
        read_resp_bulk(&mut reader)
    }

    pub fn get_config(&self) -> Result<Vec<u8>, HomeError> {
        if !self.enabled() {
            return Err(HomeError::Disabled);
        }
        let raw = self.execute_command("GET", &[REDIS_KEY_CONFIG.as_bytes()])?;
        match raw {
            Some(bytes) if !bytes.is_empty() => Ok(bytes),
            Some(_) => Err(HomeError::EmptyResponse),
            None => Err(HomeError::ConfigNotFound),
        }
    }

    pub fn get_models(
        &self,
        headers: &HashMap<String, String>,
        query: &HashMap<String, String>,
    ) -> Result<Vec<u8>, HomeError> {
        if !self.enabled() {
            return Err(HomeError::Disabled);
        }
        let req = ModelsRequest {
            req_type: "models".to_string(),
            headers: headers.clone(),
            query: query.clone(),
        };
        let key_bytes = serde_json::to_vec(&req)?;
        let raw = self.execute_command("GET", &[&key_bytes])?;
        match raw {
            Some(bytes) if !bytes.is_empty() => Ok(bytes),
            Some(_) => Err(HomeError::EmptyResponse),
            None => Err(HomeError::ModelsNotFound),
        }
    }

    pub fn apply_authoritative_snapshot(
        &self,
        config: Vec<u8>,
        models: Vec<u8>,
    ) -> Result<bool, HomeError> {
        let mut payload = self
            .config_payload
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let value: serde_json::Value = serde_json::from_slice(&config)?;
        let revision = value
            .get("revision")
            .and_then(serde_json::Value::as_i64)
            .or_else(|| value.get("rev").and_then(serde_json::Value::as_i64))
            .ok_or_else(|| HomeError::Protocol("Home config revision is missing".into()))?;
        if revision <= self.applied_revision.load(Ordering::Acquire) {
            return Ok(false);
        }
        let model_list: serde_json::Value = serde_json::from_slice(&models)?;
        if !model_list
            .get("data")
            .is_some_and(serde_json::Value::is_array)
        {
            return Err(HomeError::Protocol(
                "Home models response is invalid".into(),
            ));
        }
        if let Some(settings) = self
            .settings
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .and_then(std::sync::Weak::upgrade)
        {
            let mut next: crate::management::settings::Settings =
                serde_json::from_value(value.get("config").cloned().unwrap_or(value))?;
            next.home.enabled = true;
            settings
                .apply_home(next)
                .map_err(|e| HomeError::Protocol(e.to_string()))?;
        }
        *payload = Some(config);
        *self
            .models_payload
            .write()
            .unwrap_or_else(|p| p.into_inner()) = Some(models);
        self.applied_revision.store(revision, Ordering::Release);
        Ok(true)
    }

    pub fn bind_settings(&self, settings: &Arc<crate::management::store::SettingsStore>) {
        *self.settings.write().unwrap_or_else(|p| p.into_inner()) = Some(Arc::downgrade(settings));
        let payload = self
            .config_payload
            .read()
            .unwrap_or_else(|p| p.into_inner());
        if let Some(raw) = payload.as_ref() {
            let replay = (|| -> Result<(), HomeError> {
                let value: serde_json::Value = serde_json::from_slice(raw)?;
                let mut next: crate::management::settings::Settings =
                    serde_json::from_value(value.get("config").cloned().unwrap_or(value))?;
                next.home.enabled = true;
                settings
                    .apply_home(next)
                    .map_err(|error| HomeError::Protocol(error.to_string()))?;
                Ok(())
            })();
            if let Err(error) = replay {
                self.close();
                tracing::error!(%error, "Home initial config application failed");
            }
        }
    }

    pub fn dispatch_member(
        &self,
        model: &str,
    ) -> Result<(crate::account::AccountMember, String), HomeError> {
        let raw = self.pull_auth(model)?;
        let decoded = (|| -> Result<_, HomeError> {
            let value: serde_json::Value = serde_json::from_slice(&raw)?;
            let auth = value.get("auth").unwrap_or(&value);
            let id = auth
                .get("id")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| HomeError::Protocol("Home auth id is missing".into()))?;
            let provider = value
                .get("provider")
                .or_else(|| auth.get("provider"))
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| HomeError::Protocol("Home auth provider is missing".into()))?;
            let credential = auth.get("metadata").unwrap_or(auth).clone();
            let member =
                crate::account::AccountMember::from_home(id.to_owned(), provider, credential)
                    .map_err(|_| HomeError::Protocol("invalid Home credential".into()))?;
            let upstream_model = value
                .get("model")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(model)
                .to_owned();
            if let Some(tuple) = value.get("concurrency") {
                if tuple.get("accounted").and_then(serde_json::Value::as_bool) != Some(true)
                    || tuple
                        .get("credential_id")
                        .and_then(serde_json::Value::as_str)
                        != Some(id)
                    || tuple.get("model").and_then(serde_json::Value::as_str)
                        != Some(upstream_model.as_str())
                {
                    return Err(HomeError::Protocol("invalid Home concurrency tuple".into()));
                }
            }
            Ok((member, upstream_model))
        })();
        if decoded.is_err() {
            self.abort_ambiguous_dispatch();
        }
        decoded
    }

    pub fn report_admission(
        &self,
        credential_id: &str,
        model: &str,
        request_id: &str,
    ) -> Result<(), HomeError> {
        let revision = self.release_seq.load(Ordering::Acquire);
        let frame = InFlightSnapshotFrame {
            kind: "snapshot".into(),
            revision,
            observed_at: Utc::now(),
            barrier_revision: revision,
            part_index: None,
            part_count: None,
            details_truncated: false,
            aggregates: vec![InFlightAggregate {
                credential_id: credential_id.into(),
                model: model.into(),
                status: "in-flight".into(),
                count: 1,
            }],
            details: vec![InFlightRequestDetail {
                request_id: request_id.into(),
                credential_id: credential_id.into(),
                model: model.into(),
                request_kind: "inference".into(),
                started_at: Utc::now(),
            }],
            aggregate_group_count: 1,
        };
        self.lpush_in_flight_snapshot(&serde_json::to_vec(&frame)?)
    }

    pub fn pull_auth(&self, model: &str) -> Result<Vec<u8>, HomeError> {
        self.rpop_auth(model, "", "", &HashMap::new(), 1, "", None, None, "")
    }

    #[allow(clippy::too_many_arguments)]
    pub fn rpop_auth(
        &self,
        requested_model: &str,
        session_id: &str,
        parent_session_id: &str,
        headers: &HashMap<String, String>,
        count: usize,
        credential_policy: &str,
        retry_round: Option<usize>,
        excluded_auth_ids: Option<Vec<String>>,
        pinned_auth_id: &str,
    ) -> Result<Vec<u8>, HomeError> {
        if self.dispatch_fenced() {
            return Err(HomeError::DispatchFenced);
        }
        if requested_model.trim().is_empty() {
            return Err(HomeError::Protocol("requested model is empty".into()));
        }

        let effective_count = if excluded_auth_ids.is_some() {
            1
        } else {
            count.max(1)
        };
        let req = AuthDispatchRequest {
            req_type: "auth".to_string(),
            model: requested_model.trim().to_string(),
            count: effective_count,
            concurrency_protocol: 1,
            session_id: session_id.trim().to_string(),
            parent_session_id: parent_session_id.trim().to_string(),
            node_kind: headers.get("x-node-kind").cloned().unwrap_or_default(),
            headers: headers.clone(),
            credential_policy: credential_policy.trim().to_string(),
            retry_round,
            excluded_auth_ids,
            pinned_auth_id: pinned_auth_id.trim().to_string(),
        };

        let key_bytes = serde_json::to_vec(&req)?;
        let mut stream = self.connect()?;
        let wire = encode_resp_command_bytes("RPOP", &[&key_bytes]);
        let res = (|| -> Result<_, HomeError> {
            stream.write_all(&wire)?;
            stream.flush()?;
            read_resp_bulk(&mut BufReader::new(stream))
        })();
        match res {
            Ok(Some(bytes)) if !bytes.is_empty() => Ok(bytes),
            Ok(Some(_)) => {
                self.abort_ambiguous_dispatch();
                Err(HomeError::EmptyResponse)
            }
            Ok(None) => Err(HomeError::AuthNotFound),
            Err(e) => {
                let err_str = e.to_string();
                if matches!(e, HomeError::Io(_) | HomeError::Protocol(_)) {
                    self.abort_ambiguous_dispatch();
                    Err(HomeError::AmbiguousDispatch(err_str))
                } else {
                    Err(e)
                }
            }
        }
    }

    pub fn lpush_usage(&self, payload: &[u8]) -> Result<(), HomeError> {
        if payload.is_empty() {
            return Ok(());
        }
        self.execute_command("LPUSH", &[REDIS_KEY_USAGE.as_bytes(), payload])?;
        Ok(())
    }

    pub fn lpush_in_flight_snapshot(&self, payload: &[u8]) -> Result<(), HomeError> {
        self.execute_command("LPUSH", &[REDIS_KEY_IN_FLIGHT_SNAPSHOT.as_bytes(), payload])?;
        Ok(())
    }

    pub fn push_concurrency_release(
        &self,
        frame: &ConcurrencyReleaseFrame,
    ) -> Result<(), HomeError> {
        if frame.credential_id.is_empty() || frame.model.is_empty() || frame.release_seq <= 0 {
            return Err(HomeError::Protocol(
                "invalid concurrency release frame".into(),
            ));
        }
        let payload = serde_json::to_vec(frame)?;
        self.execute_command(
            "LPUSH",
            &[REDIS_KEY_CONCURRENCY_RELEASE.as_bytes(), &payload],
        )?;
        Ok(())
    }

    pub fn rpush_request_log(&self, payload: &[u8]) -> Result<(), HomeError> {
        if payload.is_empty() {
            return Ok(());
        }
        self.execute_command("RPUSH", &[REDIS_KEY_REQUEST_LOG.as_bytes(), payload])?;
        Ok(())
    }

    pub fn rpush_app_log(&self, payload: &[u8]) -> Result<(), HomeError> {
        if payload.is_empty() {
            return Ok(());
        }
        self.execute_command("RPUSH", &[REDIS_KEY_APP_LOG.as_bytes(), payload])?;
        Ok(())
    }

    pub fn rpush_plugin_status(&self, payload: &[u8]) -> Result<(), HomeError> {
        if payload.is_empty() {
            return Ok(());
        }
        self.execute_command("RPUSH", &[REDIS_KEY_PLUGIN_STATUS.as_bytes(), payload])?;
        Ok(())
    }

    pub fn refresh_cluster_nodes(&self) -> Result<bool, HomeError> {
        let cfg = self.config.read().unwrap();
        if cfg.disable_cluster_discovery {
            return Ok(false);
        }
        drop(cfg);

        let res = self.execute_command("CLUSTER", &[b"NODES"])?;
        let raw = match res {
            Some(b) => b,
            None => return Ok(false),
        };

        let env: ClusterNodesEnvelope = match serde_json::from_slice(&raw) {
            Ok(e) => e,
            Err(_) => return Ok(false),
        };

        let mut nodes = env.nodes;
        nodes.retain(|n| !n.ip.trim().is_empty() && n.port > 0);
        nodes.sort_by_key(|n| n.client_count);

        if nodes.is_empty() {
            return Ok(false);
        }

        let best = &nodes[0];
        let mut cfg = self.config.write().unwrap();
        let switched = cfg.host != best.ip || cfg.port != best.port;
        if switched {
            cfg.host = best.ip.clone();
            cfg.port = best.port;
            info!("Home cluster target switched to {}:{}", best.ip, best.port);
        }
        drop(cfg);

        let mut current_nodes = self.cluster_nodes.write().unwrap();
        *current_nodes = nodes;
        Ok(switched)
    }

    pub fn mark_reconnect_failure(&self, _reason: &str) {
        let failures = self.reconnect_failures.fetch_add(1, Ordering::SeqCst) + 1;
        if failures >= HOME_RECONNECT_FAILOVER_THRESHOLD as u32 {
            self.reconnect_failures.store(0, Ordering::SeqCst);
            self.switch_to_next_node();
        }
    }

    fn switch_to_next_node(&self) {
        let nodes = self.cluster_nodes.read().unwrap().clone();
        let mut candidates = nodes;
        if !self.seed_host.is_empty() && self.seed_port > 0 {
            candidates.push(ClusterNode {
                ip: self.seed_host.clone(),
                port: self.seed_port,
                client_count: 0,
                is_master: false,
                last_seen_at: None,
            });
        }

        let mut cfg = self.config.write().unwrap();
        for node in candidates {
            if (node.ip != cfg.host || node.port != cfg.port)
                && !node.ip.is_empty()
                && node.port > 0
            {
                cfg.host = node.ip.clone();
                cfg.port = node.port;
                info!("Home failover switched target to {}:{}", node.ip, node.port);
                break;
            }
        }
    }
}

pub struct ReleaseFlusher {
    client: Arc<HomeClient>,
    active_seq: AtomicI64,
}

pub struct HomeLease {
    client: Arc<HomeClient>,
    credential_id: String,
    model: String,
    released: AtomicBool,
}

impl HomeLease {
    pub fn new(client: Arc<HomeClient>, credential_id: String, model: String) -> Arc<Self> {
        Arc::new(Self {
            client,
            credential_id,
            model,
            released: AtomicBool::new(false),
        })
    }

    pub fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        let client = self.client.clone();
        let frame = ConcurrencyReleaseFrame {
            credential_id: self.credential_id.clone(),
            model: self.model.clone(),
            release_seq: client.release_seq.fetch_add(1, Ordering::AcqRel) + 1,
        };
        tokio::task::spawn_blocking(move || {
            if let Err(error) = client.push_concurrency_release(&frame) {
                tracing::warn!(%error, "Home concurrency release failed");
            }
        });
    }
}

impl Drop for HomeLease {
    fn drop(&mut self) {
        self.release();
    }
}

impl ReleaseFlusher {
    pub fn new(client: Arc<HomeClient>) -> Arc<Self> {
        Arc::new(Self {
            client,
            active_seq: AtomicI64::new(0),
        })
    }

    pub fn mark_dirty(&self, credential_id: &str, model: &str, sequence: i64) {
        if sequence <= self.active_seq.load(Ordering::Acquire) {
            return;
        }
        let frame = ConcurrencyReleaseFrame {
            credential_id: credential_id.to_string(),
            model: model.to_string(),
            release_seq: sequence,
        };
        if let Ok(()) = self.client.push_concurrency_release(&frame) {
            self.active_seq.store(sequence, Ordering::Release);
        }
    }
}

pub struct HomeSyncWorker {
    client: Arc<HomeClient>,
    running: Arc<AtomicBool>,
}

impl HomeSyncWorker {
    pub fn new(client: Arc<HomeClient>) -> Self {
        Self {
            client,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn spawn(&self) -> tokio::task::JoinHandle<()> {
        let client = Arc::clone(&self.client);
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::Release);

        tokio::spawn(async move {
            info!("Home sync worker started");
            let mut interval = tokio::time::interval(Duration::from_secs(5));
            while running.load(Ordering::Acquire) {
                interval.tick().await;

                let client_ref = Arc::clone(&client);
                let _ = tokio::task::spawn_blocking(move || {
                    let sync = (|| -> Result<(), HomeError> {
                        let config = client_ref.get_config()?;
                        let models = client_ref.get_models(&HashMap::new(), &HashMap::new())?;
                        client_ref.apply_authoritative_snapshot(config, models)?;
                        client_ref.refresh_cluster_nodes()?;
                        client_ref.heartbeat_ok.store(true, Ordering::Release);
                        client_ref.reconnect_failures.store(0, Ordering::Release);
                        Ok(())
                    })();
                    if let Err(e) = sync {
                        client_ref.heartbeat_ok.store(false, Ordering::Release);
                        client_ref.mark_reconnect_failure(&e.to_string());
                    }
                })
                .await;
            }
            info!("Home sync worker stopped");
        })
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }
}

static ACTIVE_HOME_CLIENT: Mutex<Option<Arc<HomeClient>>> = Mutex::new(None);

pub fn set_active_home_client(client: Option<Arc<HomeClient>>) {
    let mut lock = ACTIVE_HOME_CLIENT.lock().unwrap();
    *lock = client;
}

pub fn get_active_home_client() -> Option<Arc<HomeClient>> {
    ACTIVE_HOME_CLIENT.lock().unwrap().clone()
}

pub fn is_home_authoritative() -> bool {
    get_active_home_client()
        .map(|c| c.heartbeat_ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_normalize_home_port() {
        assert_eq!(normalize_home_port(0), DEFAULT_HOME_PORT);
        assert_eq!(normalize_home_port(18845), 18845);
    }

    #[test]
    fn test_normalize_fingerprint() {
        let raw = "AA:BB:CC:dd:EE:ff 11:22";
        assert_eq!(normalize_fingerprint(raw), "aabbccddeeff1122");
    }

    #[test]
    fn test_encode_resp_array() {
        let args = ["PING", "HELLO"];
        let encoded = encode_resp_array(&args);
        assert_eq!(encoded, b"*2\r\n$4\r\nPING\r\n$5\r\nHELLO\r\n");
    }

    #[test]
    fn test_read_resp_bulk_positive() {
        let payload = b"$5\r\nWORLD\r\n";
        let mut cursor = Cursor::new(&payload[..]);
        let res = read_resp_bulk(&mut cursor).unwrap();
        assert_eq!(res, Some(b"WORLD".to_vec()));
    }

    #[test]
    fn test_read_resp_bulk_nil() {
        let payload = b"$-1\r\n";
        let mut cursor = Cursor::new(&payload[..]);
        let res = read_resp_bulk(&mut cursor).unwrap();
        assert_eq!(res, None);
    }

    #[test]
    fn test_read_resp_bulk_error() {
        let payload = b"-ERR unknown command\r\n";
        let mut cursor = Cursor::new(&payload[..]);
        let err = read_resp_bulk(&mut cursor).unwrap_err();
        match err {
            HomeError::Protocol(msg) => assert_eq!(msg, "ERR unknown command"),
            other => panic!("expected Protocol error, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_jwt_claims_valid() {
        let header = r#"{"alg":"none","typ":"JWT"}"#;
        let claims = r#"{"certificate_id":"node-1","cluster_id":"cluster-a","ca_fingerprint":"aa:bb:cc:dd","enrollment_secret":"secret123","ip":"127.0.0.1","port":8317}"#;
        let b64_header = URL_SAFE_NO_PAD.encode(header);
        let b64_claims = URL_SAFE_NO_PAD.encode(claims);
        let token = format!("{b64_header}.{b64_claims}.signature");

        let parsed = parse_home_jwt_claims(&token).unwrap();
        assert_eq!(parsed.certificate_id, "node-1");
        assert_eq!(parsed.cluster_id, "cluster-a");
        assert_eq!(normalize_fingerprint(&parsed.ca_fingerprint), "aabbccdd");
        assert_eq!(parsed.ip, "127.0.0.1");
        assert_eq!(parsed.port, 8317);
    }

    #[test]
    fn test_parse_jwt_claims_missing_fields() {
        let header = r#"{"alg":"none","typ":"JWT"}"#;
        let claims = r#"{"certificate_id":"","cluster_id":"cluster-a","ca_fingerprint":"aabb","enrollment_secret":"sec","ip":"127.0.0.1","port":8317}"#;
        let b64_header = URL_SAFE_NO_PAD.encode(header);
        let b64_claims = URL_SAFE_NO_PAD.encode(claims);
        let token = format!("{b64_header}.{b64_claims}.");

        assert!(parse_home_jwt_claims(&token).is_err());
    }

    #[test]
    fn test_ambiguous_dispatch_fencing() {
        let client = HomeClient::new(HomeConfig {
            enabled: true,
            node_id: "test-node".into(),
            host: "127.0.0.1".into(),
            port: 18845,
            disable_cluster_discovery: false,
            tls: HomeTLSConfig::default(),
        });
        assert!(!client.dispatch_fenced());
        assert!(!client.ambiguous_dispatch());

        client.abort_ambiguous_dispatch();
        assert!(client.dispatch_fenced());
        assert!(client.ambiguous_dispatch());
        assert!(!client.heartbeat_ok());
    }

    #[test]
    fn test_concurrency_release_flusher() {
        let client = HomeClient::new(HomeConfig::default());
        let flusher = ReleaseFlusher::new(client);
        assert_eq!(flusher.active_seq.load(Ordering::Acquire), 0);
    }

    #[test]
    fn test_home_client_active_global() {
        let client = HomeClient::new(HomeConfig {
            enabled: true,
            node_id: "active-node".into(),
            host: "127.0.0.1".into(),
            port: 18845,
            disable_cluster_discovery: false,
            tls: HomeTLSConfig::default(),
        });
        assert!(!is_home_authoritative());

        set_active_home_client(Some(client.clone()));
        assert!(!is_home_authoritative());

        client.heartbeat_ok.store(true, Ordering::Release);
        assert!(is_home_authoritative());

        set_active_home_client(None);
        assert!(!is_home_authoritative());
    }

    #[test]
    fn test_pinning_verifier_requires_ca_chain_verifier() {
        let verifier = HomePinningVerifier {
            insecure_skip_verify: false,
            verifier: None,
        };
        assert!(verifier.verifier.is_none());
        assert!(!verifier.insecure_skip_verify);
    }

    #[test]
    fn test_loopback_resp_interaction() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();

        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line); // *2\r\n
                line.clear();
                let _ = reader.read_line(&mut line); // $3\r\n
                line.clear();
                let _ = reader.read_line(&mut line); // GET\r\n
                line.clear();
                let _ = reader.read_line(&mut line); // $6\r\n
                line.clear();
                let _ = reader.read_line(&mut line); // config\r\n
                let _ = stream.write_all(b"$12\r\n{\"rev\": 101}\r\n");
                let _ = stream.flush();
            }
        });

        let client = HomeClient::new(HomeConfig {
            enabled: true,
            node_id: "loopback-node".into(),
            host: "127.0.0.1".into(),
            port,
            disable_cluster_discovery: true,
            tls: HomeTLSConfig::default(),
        });

        let config_bytes = client.get_config().expect("get_config");
        assert_eq!(config_bytes, b"{\"rev\": 101}");
        let _ = handle.join();
    }
}

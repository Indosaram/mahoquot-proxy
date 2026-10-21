fn init_storage_manager(
    auth_dir: &std::path::Path,
) -> Option<Arc<mahoquot_registry::store::StorageManager>> {
    use mahoquot_registry::store::{
        BackendConfig, GitStore, ObjectStore, PostgresStore, StorageBackend, StorageManager,
        StoreOptions,
    };

    let options = StoreOptions::from_env(auth_dir).ok()?;
    let (backend, spool): (Arc<dyn StorageBackend>, std::path::PathBuf) = match options.config {
        BackendConfig::Postgres { dsn, schema, spool } => {
            info!(dsn = %dsn, schema = ?schema, spool = %spool.display(), "enabling postgres-backed storage");
            (Arc::new(PostgresStore::new(dsn, schema)), spool)
        }
        BackendConfig::Object {
            endpoint,
            bucket,
            access_key,
            secret_key,
            prefix,
            spool,
        } => {
            info!(endpoint = %endpoint, bucket = %bucket, prefix = %prefix, spool = %spool.display(), "enabling object-backed storage");
            let store = ObjectStore::new(endpoint, bucket, access_key, secret_key, prefix).ok()?;
            (Arc::new(store), spool)
        }
        BackendConfig::Git { repository, spool } => {
            info!(repository = %repository.display(), spool = %spool.display(), "enabling git-backed storage");
            let remote = std::env::var("GITSTORE_GIT_URL").unwrap_or_default();
            let branch = std::env::var("GITSTORE_GIT_BRANCH").ok();
            (Arc::new(GitStore::new(repository, remote, branch)), spool)
        }
        BackendConfig::Local { .. } => return None,
    };

    StorageManager::new(backend, spool).ok().map(Arc::new)
}

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand};
use mahoquot_gateway::account::load_account_members;
use mahoquot_gateway::config::{
    resolve_bind_addr, should_warn_for_unauthenticated_bind, GatewayConfig,
};
use mahoquot_gateway::inbound::ApiKeys;
use mahoquot_gateway::management::settings::Settings;
use mahoquot_gateway::routes::create_app;
use mahoquot_gateway::server::{run_protocol_server, ProtocolListener};
use mahoquot_gateway::state::AppState;
use mahoquot_types::Strategy;
use tokio::net::TcpListener;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "mahoquot-gateway",
    author,
    version,
    about = "Mahoquot high-concurrency LLM inference proxy and router"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    #[command(flatten)]
    pub serve_args: ServeArgs,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run the proxy server (default)
    Serve(ServeArgs),
    /// Validate authentication directory, accounts, and configuration offline
    Doctor {
        /// Directory storing provider credential JSON files
        #[arg(long, env = "AUTH_DIR")]
        auth_dir: PathBuf,

        /// Path to optional persistent YAML settings file
        #[arg(long, env = "CONFIG_PATH")]
        config: Option<PathBuf>,
    },
    /// Dump status table of all configured provider accounts
    Accounts {
        /// Directory storing provider credential JSON files
        #[arg(long, env = "AUTH_DIR")]
        auth_dir: PathBuf,
    },
    /// Discover local AI gateways and CPA instances on the LAN
    Discover {
        /// Timeout in seconds for LAN discovery
        #[arg(long, default_value_t = 3)]
        timeout: u64,

        /// Output discovered gateways in JSON format
        #[arg(long)]
        json: bool,

        /// DNS-SD service type for LAN discovery
        #[arg(long, default_value = "_ai-gateway._tcp")]
        service_type: String,
    },
    /// Interactive terminal management UI
    Tui {
        /// Base URL of management API for TUI client mode
        #[arg(
            long,
            env = "MANAGEMENT_BASE_URL",
            default_value = "http://127.0.0.1:18801"
        )]
        management_base_url: String,

        /// Management API secret key / password
        #[arg(long, env = "MANAGEMENT_PASSWORD")]
        password: Option<String>,

        /// Start an embedded local server in standalone mode
        #[arg(long)]
        standalone: bool,
    },
}

#[derive(Args, Debug, Clone, Default)]
pub struct ServeArgs {
    /// TCP port the gateway listens on
    #[arg(long, env = "GATEWAY_PORT", default_value_t = 18801)]
    pub port: u16,

    /// Address the gateway listens on (BIND_ADDR takes precedence)
    #[arg(long)]
    pub bind: Option<String>,

    /// Directory storing provider credential JSON files
    #[arg(long, env = "AUTH_DIR")]
    pub auth_dir: Option<PathBuf>,

    /// Routing strategy (round_robin or fill_first)
    #[arg(long, env = "STRATEGY", default_value = "round_robin")]
    pub strategy: String,

    /// Maximum failover retry attempts across accounts
    #[arg(long, env = "MAX_FAILOVER", default_value_t = 3)]
    pub max_failover: usize,

    /// Path to persistent YAML settings file
    #[arg(long, env = "CONFIG_PATH")]
    pub config: Option<PathBuf>,

    /// Comma-separated inbound bearer API keys
    #[arg(long, env = "API_KEYS")]
    pub api_keys: Option<String>,

    /// Model override/mapping configuration
    #[arg(long, env = "MODELS")]
    pub models: Option<String>,

    /// OAuth token refresh endpoint URL
    #[arg(long, env = "REFRESH_URL")]
    pub refresh_url: Option<String>,

    /// Enable automatic background token refreshing
    #[arg(long, env = "AUTH_REFRESH", default_value = "true")]
    pub auth_refresh: String,

    /// Interval in seconds for polling provider usage endpoints
    #[arg(long, env = "USAGE_POLL_SECS", default_value_t = 120)]
    pub usage_poll_secs: u64,

    /// Logging filter level
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    pub log_level: String,

    /// Ceiling on concurrently admitted upstream inference requests. Held for
    /// the downstream response body lifetime; requests past it get an immediate
    /// retryable 503. Sized far above real concurrency.
    #[arg(
        long,
        env = "MAX_CONCURRENT_INFERENCE",
        default_value_t = mahoquot_gateway::state::MAX_CONCURRENT_INFERENCE_REQUESTS
    )]
    pub max_concurrent_inference: usize,
}

impl ServeArgs {
    pub fn build_config(self) -> anyhow::Result<GatewayConfig> {
        let auth_dir = self.auth_dir.ok_or_else(|| {
            anyhow::anyhow!("AUTH_DIR environment variable or --auth-dir flag is required")
        })?;

        let strategy = match self.strategy.as_str() {
            "fill_first" | "fill-first" => Strategy::FillFirst,
            _ => Strategy::StrictRoundRobin,
        };

        let api_keys = match self.api_keys {
            Some(ref val) if !val.is_empty() => ApiKeys::from_env_value(val),
            _ => ApiKeys::default(),
        };

        let refresh_url = self
            .refresh_url
            .unwrap_or_else(|| mahoquot_providers::refresh::REFRESH_TOKEN_URL.to_string());

        let auth_refresh_enabled = !matches!(self.auth_refresh.as_str(), "false" | "0");

        let config_path = self.config.unwrap_or_else(|| auth_dir.join("config.yaml"));

        let usage_poll_secs = if self.usage_poll_secs > 0 {
            self.usage_poll_secs
        } else {
            120
        };

        let max_concurrent_inference = if self.max_concurrent_inference > 0 {
            self.max_concurrent_inference
        } else {
            mahoquot_gateway::state::MAX_CONCURRENT_INFERENCE_REQUESTS
        };

        Ok(GatewayConfig {
            port: self.port,
            auth_dir,
            strategy,
            max_failover: self.max_failover,
            log_level: self.log_level,
            api_keys,
            models_env: self.models,
            refresh_url,
            auth_refresh_enabled,
            usage_poll_secs,
            config_path,
            catalog_cache_path: std::env::var("MAHOQUOT_CACHE_DIR")
                .ok()
                .map(|dir| PathBuf::from(dir).join("models-v1.signed.json")),
            history_queue_capacity: 1024,
            history_batch_size: 64,
            max_concurrent_inference,
            captcha_config_url: std::env::var("ZCODE_CAPTCHA_CONFIG_URL")
                .ok()
                .filter(|value| !value.trim().is_empty()),
            captcha_solver_bin: std::env::var("MAHOQUOT_CAPTCHA_SOLVER_BIN")
                .ok()
                .map(PathBuf::from)
                .filter(|value| !value.as_os_str().is_empty()),
        })
    }
}

fn run_doctor(auth_dir: PathBuf, config: Option<PathBuf>) -> anyhow::Result<()> {
    println!("=== Mahoquot Gateway Doctor ===");
    println!("Auth directory: {}", auth_dir.display());

    if !auth_dir.is_dir() {
        eprintln!(
            "Error: auth directory '{}' does not exist or is not a directory",
            auth_dir.display()
        );
        std::process::exit(1);
    }

    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let members = match load_account_members(&auth_dir) {
        Ok(m) => m,
        Err(e) => {
            eprintln!(
                "Error loading accounts from '{}': {}",
                auth_dir.display(),
                e
            );
            std::process::exit(1);
        }
    };

    println!("Found {} account(s):", members.len());
    for (i, member) in members.iter().enumerate() {
        let has_access = !member.access_token().is_empty();
        let has_refresh = !member.refresh_token().is_empty();
        let expired = member.is_expired(now_unix);
        println!(
            "  [{}] id: {:<20} provider: {:<12} access_token: {:<10} refresh_token: {:<10} expired: {}",
            i + 1,
            member.id,
            member.provider_name(),
            if has_access { "present" } else { "none" },
            if has_refresh { "present" } else { "none" },
            if expired { "YES" } else { "no" }
        );
    }

    let cfg_path = config.unwrap_or_else(|| auth_dir.join("config.yaml"));
    if cfg_path.exists() {
        print!("Config file '{}': ", cfg_path.display());
        match std::fs::read_to_string(&cfg_path) {
            Ok(content) => match serde_yaml::from_str::<Settings>(&content) {
                Ok(_) => println!("valid YAML settings"),
                Err(e) => {
                    println!("INVALID ({})", e);
                    std::process::exit(1);
                }
            },
            Err(e) => {
                println!("UNREADABLE ({})", e);
                std::process::exit(1);
            }
        }
    } else {
        println!("Config file: None (default settings will be used)");
    }

    println!("\nDoctor check completed successfully.");
    Ok(())
}

fn run_accounts(auth_dir: PathBuf) -> anyhow::Result<()> {
    if !auth_dir.is_dir() {
        eprintln!(
            "Error: auth directory '{}' does not exist or is not a directory",
            auth_dir.display()
        );
        std::process::exit(1);
    }

    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let members = load_account_members(&auth_dir)?;

    println!("{:-<78}", "");
    println!(
        "{:<24} {:<14} {:<12} {:<10} {:<10}",
        "ID", "PROVIDER", "STATUS", "EXPIRED", "TOKENS"
    );
    println!("{:-<78}", "");
    for member in &members {
        let has_tokens = !member.access_token().is_empty() || !member.refresh_token().is_empty();
        let expired = member.is_expired(now_unix);
        println!(
            "{:<24} {:<14} {:<12} {:<10} {:<10}",
            member.id,
            member.provider_name(),
            "ready",
            if expired { "yes" } else { "no" },
            if has_tokens { "ok" } else { "none" }
        );
    }
    println!("{:-<78}", "");
    println!("Total accounts: {}", members.len());

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Both aws-lc-rs and ring are compiled into rustls through dependencies,
    // so rustls cannot pick a process default on its own; libraries that use
    // the default (webrtc DTLS for realtime media) would panic. Install the
    // ring provider every explicit config in this binary already uses.
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let cli = Cli::parse();

    let serve_args = match cli.command {
        Some(Commands::Serve(args)) => args,
        Some(Commands::Doctor { auth_dir, config }) => return run_doctor(auth_dir, config),
        Some(Commands::Accounts { auth_dir }) => return run_accounts(auth_dir),
        Some(Commands::Discover {
            timeout,
            json,
            service_type,
        }) => {
            let browser = mahoquot_gateway::discovery::RealBrowser::new();
            let discovered = browser
                .browse_with_timeout(
                    &service_type,
                    mahoquot_gateway::discovery::DEFAULT_DOMAIN,
                    std::time::Duration::from_secs(timeout.max(1)),
                )
                .unwrap_or_default();
            if json {
                println!("{}", serde_json::to_string_pretty(&discovered)?);
            } else if discovered.is_empty() {
                println!(
                    "Scanning LAN for AI Gateways ({})... (timeout {}s)",
                    service_type, timeout
                );
                println!("\nNo AI gateways found on local network.");
            } else {
                println!(
                    "\nFound {} AI Gateway(s) on local network:\n",
                    discovered.len()
                );
                for (i, gw) in discovered.iter().enumerate() {
                    println!(
                        "[{}] {} (Product: {}, Port: {})",
                        i + 1,
                        gw.instance_name,
                        gw.product,
                        gw.port
                    );
                }
            }
            return Ok(());
        }
        Some(Commands::Tui {
            management_base_url,
            password,
            standalone,
        }) => {
            let client = mahoquot_gateway::tui::ManagementClient::new(
                management_base_url,
                password.unwrap_or_default(),
            );
            let state = mahoquot_gateway::tui::TuiState::new(client, standalone, None);
            mahoquot_gateway::tui::run_interactive_tui(state).await?;
            return Ok(());
        }
        None => cli.serve_args,
    };

    let bind_addr = resolve_bind_addr(serve_args.bind.clone(), std::env::var("BIND_ADDR").ok());
    let config = serve_args.build_config()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level)),
        )
        // Colour only an interactive terminal: piped/supervised logs (and
        // NO_COLOR) stay plain so field values like `decision="deny"` are
        // matchable verbatim.
        .with_ansi(
            std::io::IsTerminal::is_terminal(&std::io::stdout())
                && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()),
        )
        .init();

    info!(
        port = config.port,
        auth_dir = %config.auth_dir.display(),
        strategy = ?config.strategy,
        max_failover = config.max_failover,
        "starting mahoquot-gateway"
    );

    let mut config = config;
    let storage_mgr = if std::env::var("HOME_JWT").is_ok_and(|jwt| !jwt.trim().is_empty()) {
        None
    } else {
        init_storage_manager(&config.auth_dir)
    };
    if let Some(ref mgr) = storage_mgr {
        // If the backend has no config yet, seed from initial config_path if present
        if mgr.get("config/config.yaml").ok().flatten().is_none() && config.config_path.exists() {
            if let Ok(bytes) = std::fs::read(&config.config_path) {
                let _ = mgr.put("config/config.yaml", &bytes);
            }
        }
        // If the backend has no auth files yet, seed from initial auth_dir
        if mgr.list("auths").map(|l| l.is_empty()).unwrap_or(false) && config.auth_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&config.auth_dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_file() {
                        let name = path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or_default();
                        if name.ends_with(".json") && !name.starts_with('.') {
                            if let Ok(bytes) = std::fs::read(&path) {
                                let _ = mahoquot_gateway::account::save_account_to_store(
                                    mgr, name, &bytes,
                                );
                            }
                        }
                    }
                }
            }
        }
        let _ = mgr.sync_all_to_spool();
        config.auth_dir = mgr.auth_dir();
        let _ = std::fs::create_dir_all(&config.auth_dir);
        if mgr.config_path().exists() {
            config.config_path = mgr.config_path();
        }
    }
    let mut home_sync_worker = None;
    if let Ok(jwt) = std::env::var("HOME_JWT") {
        if !jwt.trim().is_empty() {
            let home_config = mahoquot_gateway::home::config_from_jwt(&jwt, None)
                .map_err(|e| anyhow::anyhow!("invalid HOME_JWT: {e:?}"))?;
            let client = mahoquot_gateway::home::HomeClient::new(home_config);
            let worker = mahoquot_gateway::home::HomeSyncWorker::new(client.clone());
            let handle = worker.spawn();
            home_sync_worker = Some((worker, handle));
            mahoquot_gateway::home::set_active_home_client(Some(client));
        }
    }

    if let Some(ref mgr) = storage_mgr {
        mahoquot_gateway::account::set_active_storage_manager(Some(Arc::clone(mgr)));
    }
    let mut app_state = AppState::new(&config)?;
    println!("gateway strategy={:?}", app_state.router.strategy());
    let plugin_settings = app_state.settings.current().plugins.clone();
    if plugin_settings.enabled {
        let host = Arc::new(mahoquot_gateway::plugins::loader::PluginHost::new(
            Arc::new(|_plugin_id, method, request| {
                if method == mahoquot_gateway::plugins::abi::METHOD_HOST_LOG {
                    let payload: serde_json::Value =
                        serde_json::from_slice(request).map_err(|error| {
                            mahoquot_gateway::plugins::abi::PluginError {
                                code: "invalid_host_log".to_string(),
                                message: error.to_string(),
                                retryable: false,
                                http_status: Some(400),
                            }
                        })?;
                    info!(plugin_message = %payload.get("message").and_then(serde_json::Value::as_str).unwrap_or(""), "plugin log");
                }
                Ok(serde_json::json!({"ok":true,"result":{}})
                    .to_string()
                    .into_bytes())
            }),
        ));
        let plugins_dir = if plugin_settings.dir.trim().is_empty() {
            PathBuf::from("plugins")
        } else {
            PathBuf::from(plugin_settings.dir)
        };
        mahoquot_gateway::plugins::loader::load_plugins_from_directory(&host, &plugins_dir);
        if let Ok(id) = std::env::var("MAHOQUOT_QA_PLUGIN_BUSY") {
            let extension = if cfg!(target_os = "macos") {
                ".dylib"
            } else if cfg!(target_os = "windows") {
                ".dll"
            } else {
                ".so"
            };
            let path = plugins_dir.join(format!("{id}{extension}"));
            if path.is_file() {
                let _ = host.load_plugin(&id, &path, "fixture");
            }
        }
        app_state.set_plugin_host(Some(host));
    }
    // Relay dispatch reads the Home client from AppState; mirror the active
    // process-wide client here so Home-authoritative dispatch is reachable.
    app_state.home = mahoquot_gateway::home::get_active_home_client();
    let state = Arc::new(app_state);
    let cline_catalog = mahoquot_gateway::cline_catalog::spawn(Arc::clone(&state));
    if let Some(client) = mahoquot_gateway::home::get_active_home_client() {
        client.bind_settings(&state.settings);
    }
    if let Some(ref mgr) = storage_mgr {
        state.settings.set_storage_manager(Arc::clone(mgr));
    }

    // Start discovery advertisement if enabled in settings
    let mut discovery_mgr = mahoquot_gateway::discovery::DiscoveryManager::real();
    let discovery_cfg = if let Ok(content) = std::fs::read_to_string(&config.config_path) {
        if let Ok(val) = serde_yaml::from_str::<serde_json::Value>(&content) {
            if let Some(disc) = val.get("discovery") {
                serde_json::from_value::<mahoquot_gateway::discovery::DiscoveryConfig>(disc.clone())
                    .unwrap_or_default()
            } else {
                mahoquot_gateway::discovery::DiscoveryConfig::default()
            }
        } else {
            mahoquot_gateway::discovery::DiscoveryConfig::default()
        }
    } else {
        mahoquot_gateway::discovery::DiscoveryConfig::default()
    };

    if discovery_cfg.enabled {
        let ifaces = mahoquot_gateway::discovery::query_system_interfaces();
        discovery_mgr.apply_config(
            &discovery_cfg,
            Some(&bind_addr),
            config.port,
            false,
            &ifaces,
            Some(&config.auth_dir),
        );
    }
    let listener = TcpListener::bind((bind_addr.as_str(), config.port)).await?;
    let attempts = state.api_keys.management_attempts();
    let listener = ProtocolListener::new(listener, &state.settings)
        .await?
        .with_management_context(Arc::clone(&state.settings), attempts);
    mahoquot_gateway::warmup::spawn_warmup_loop(state.clone(), std::time::Duration::from_secs(15));
    // Resume each Cline account's 24h token budget from the durable history
    // so a restart does not hand back a partially spent budget.
    mahoquot_gateway::cline_usage::seed_cline_trackers_from_history(&state);
    if should_warn_for_unauthenticated_bind(&bind_addr, state.api_keys.is_empty()) {
        warn!(
            bind_addr = %bind_addr,
            "SECURITY WARNING: gateway is listening on a non-loopback address without any API keys configured"
        );
    }
    let flush_worker = state
        .telemetry
        .spawn_flush_worker(std::time::Duration::from_secs(10));
    // Keeps the Gemini replay ledger on disk across restarts without ever
    // writing from a request task.
    let signature_ledger_worker = state.signature_ledger.spawn_persistence_worker();
    // The server owns these workers so shutdown joins them instead of leaving
    // tasks (and their `Arc<AppState>`) detached after the process stops
    // serving.
    let usage_poller = mahoquot_gateway::quota::spawn_usage_poller(
        Arc::clone(&state),
        std::time::Duration::from_secs(config.usage_poll_secs),
    );
    // Re-reads the signed model catalog on the configured interval. Without it
    // the gateway served the compile-time embedded catalog until an operator
    // triggered a refresh by hand.
    let catalog_refresh = mahoquot_gateway::registry::spawn_catalog_refresh_poller(
        Arc::clone(&state),
        mahoquot_gateway::registry::catalog_refresh_check_interval(),
    );
    mahoquot_gateway::egress_supervisor::spawn_egress_supervisor(
        state.clone(),
        std::time::Duration::from_secs(10),
    );
    let app = create_app(state.clone());

    info!(bind_addr = %bind_addr, port = config.port, "listening");

    let shutdown = state.shutdown.clone();
    let result = run_protocol_server(listener, app, async move { shutdown.notified().await }).await;
    // Cancel the poller before the final snapshot: a refresh that is still in
    // flight must not write behind the flush, and neither task may outlive the
    // server.
    if let Some((worker, handle)) = home_sync_worker {
        worker.stop();
        let _ = handle.await;
    }
    usage_poller.shutdown().await;
    catalog_refresh.shutdown().await;
    cline_catalog.shutdown().await;
    flush_worker.shutdown().await;
    discovery_mgr.shutdown();
    // Final snapshot before the process exits, so a replay that arrived inside
    // the coalescing window is not lost.
    state.signature_ledger.shutdown();
    if tokio::time::timeout(std::time::Duration::from_secs(5), signature_ledger_worker)
        .await
        .is_err()
    {
        let _ = state.signature_ledger.flush_blocking();
    }
    result
}

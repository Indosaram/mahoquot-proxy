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
use mahoquot_gateway::server::run_server;
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
    let cli = Cli::parse();

    let serve_args = match cli.command {
        Some(Commands::Serve(args)) => args,
        Some(Commands::Doctor { auth_dir, config }) => return run_doctor(auth_dir, config),
        Some(Commands::Accounts { auth_dir }) => return run_accounts(auth_dir),
        None => cli.serve_args,
    };

    let bind_addr = resolve_bind_addr(serve_args.bind.clone(), std::env::var("BIND_ADDR").ok());
    let config = serve_args.build_config()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level)),
        )
        .init();

    info!(
        port = config.port,
        auth_dir = %config.auth_dir.display(),
        strategy = ?config.strategy,
        max_failover = config.max_failover,
        "starting mahoquot-gateway"
    );

    let state = Arc::new(AppState::new(&config)?);
    if should_warn_for_unauthenticated_bind(&bind_addr, state.api_keys.is_empty()) {
        warn!(
            bind_addr = %bind_addr,
            "SECURITY WARNING: gateway is listening on a non-loopback address without any API keys configured"
        );
    }
    state
        .telemetry
        .spawn_flush_worker(std::time::Duration::from_secs(10));
    mahoquot_gateway::quota::spawn_usage_poller(
        Arc::clone(&state),
        std::time::Duration::from_secs(config.usage_poll_secs),
    );
    let app = create_app(state.clone());

    let listener = TcpListener::bind((bind_addr.as_str(), config.port)).await?;
    info!(bind_addr = %bind_addr, port = config.port, "listening");

    let shutdown = state.shutdown.clone();
    run_server(listener, app, async move { shutdown.notified().await }).await
}

use mahoquot_types::Health;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::monitor::{LastError, TtftSnapshot};
use crate::telemetry::TelemetryBucket;

#[derive(Default)]
pub struct RegistryRefreshMetrics {
    pub attempts: AtomicU64,
    pub successes: AtomicU64,
    pub errors: AtomicU64,
    pub rejected_coalesced: AtomicU64,
    pub duration_milliseconds: AtomicU64,
    pub source_embedded_fallback: AtomicU64,
    pub source_lkg_cache: AtomicU64,
    pub source_remote_signed: AtomicU64,
    pub source_discovered: AtomicU64,
    pub source_local_override: AtomicU64,
}

#[derive(Default)]
pub struct GatewayMetrics {
    pub served: AtomicU64,
    pub failed_over: AtomicU64,
    pub exposed_errors: AtomicU64,
    pub exposed_client_errors: AtomicU64,
    pub history_dropped: AtomicU64,
    pub history_database_failures: AtomicU64,
    pub registry_refresh: RegistryRefreshMetrics,
}

impl RegistryRefreshMetrics {
    pub fn render_prometheus(&self) -> String {
        format!(
            concat!(
                "# HELP mahoquot_model_registry_refresh_attempts_total Model registry refresh attempts.\n",
                "# TYPE mahoquot_model_registry_refresh_attempts_total counter\n",
                "mahoquot_model_registry_refresh_attempts_total {}\n",
                "# HELP mahoquot_model_registry_refresh_outcomes_total Model registry refresh outcomes.\n",
                "# TYPE mahoquot_model_registry_refresh_outcomes_total counter\n",
                "mahoquot_model_registry_refresh_outcomes_total{{outcome=\"success\"}} {}\n",
                "mahoquot_model_registry_refresh_outcomes_total{{outcome=\"error\"}} {}\n",
                "# HELP mahoquot_model_registry_refresh_rejections_total Model registry refresh trigger rejections.\n",
                "# TYPE mahoquot_model_registry_refresh_rejections_total counter\n",
                "mahoquot_model_registry_refresh_rejections_total{{reason=\"coalesced\"}} {}\n",
                "# HELP mahoquot_model_registry_refresh_duration_milliseconds_total Cumulative model registry refresh duration.\n",
                "# TYPE mahoquot_model_registry_refresh_duration_milliseconds_total counter\n",
                "mahoquot_model_registry_refresh_duration_milliseconds_total {}\n",
                "# HELP mahoquot_model_registry_cache_source_total Successful registry loads by source.\n",
                "# TYPE mahoquot_model_registry_cache_source_total counter\n",
                "mahoquot_model_registry_cache_source_total{{source=\"embedded_fallback\"}} {}\n",
                "mahoquot_model_registry_cache_source_total{{source=\"lkg_cache\"}} {}\n",
                "mahoquot_model_registry_cache_source_total{{source=\"remote_signed\"}} {}\n",
                "mahoquot_model_registry_cache_source_total{{source=\"discovered\"}} {}\n",
                "mahoquot_model_registry_cache_source_total{{source=\"local_override\"}} {}\n"
            ),
            self.attempts.load(Ordering::Relaxed),
            self.successes.load(Ordering::Relaxed),
            self.errors.load(Ordering::Relaxed),
            self.rejected_coalesced.load(Ordering::Relaxed),
            self.duration_milliseconds.load(Ordering::Relaxed),
            self.source_embedded_fallback.load(Ordering::Relaxed),
            self.source_lkg_cache.load(Ordering::Relaxed),
            self.source_remote_signed.load(Ordering::Relaxed),
            self.source_discovered.load(Ordering::Relaxed),
            self.source_local_override.load(Ordering::Relaxed),
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountStats {
    pub id: String,
    pub provider: String,
    /// Relay plan label; absent for every non-claude-relay account.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default)]
    pub usage: crate::usage::AccountUsage,
    pub health: HealthStats,
    pub ok: u64,
    pub fails: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub reset_at_unix_ms: Option<i64>,
    pub last_error: Option<LastError>,
    pub ttft: Option<TtftSnapshot>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum HealthStats {
    Available,
    Cooldown { until_unix_ms: i64 },
    AuthFailed,
    Disabled,
}

impl From<Health> for HealthStats {
    fn from(h: Health) -> Self {
        match h {
            Health::Available => Self::Available,
            Health::Cooldown { until_unix_ms } => Self::Cooldown { until_unix_ms },
            Health::AuthFailed => Self::AuthFailed,
            Health::Disabled => Self::Disabled,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AdminStatsResponse {
    pub uptime_secs: u64,
    pub in_flight: u64,
    pub served: u64,
    pub failed_over: u64,
    pub refreshed: u64,
    pub exposed_errors: u64,
    pub exposed_client_errors: u64,
    pub ttft: TtftSnapshot,
    pub accounts: Vec<AccountStats>,
    pub history: Vec<TelemetryBucket>,
}

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{info, warn};

use crate::proxy_policy::ProxyRuntime;
use crate::state::AppState;

pub const DEFAULT_EGRESS_ADDR: &str = "127.0.0.1:3128";
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub const MIN_RESTART_INTERVAL: Duration = Duration::from_secs(15);

pub fn spawn_egress_supervisor(state: Arc<AppState>, poll_interval: Duration) {
    tokio::spawn(async move {
        run_supervisor_loop(state, poll_interval).await;
    });
}

pub async fn run_supervisor_loop(state: Arc<AppState>, poll_interval: Duration) {
    let mut consecutive_failures = 0usize;
    let mut last_recovery_attempt = Instant::now().checked_sub(MIN_RESTART_INTERVAL).unwrap_or_else(Instant::now);
    let mut was_unhealthy = false;

    loop {
        tokio::select! {
            _ = state.shutdown.notified() => {
                info!("egress supervisor loop shutting down");
                break;
            }
            _ = tokio::time::sleep(poll_interval) => {}
        }

        let runtime = state.proxy_runtime.load();
        if !is_supervision_needed(&runtime) {
            continue;
        }

        let is_healthy = check_tcp_port(DEFAULT_EGRESS_ADDR, DEFAULT_CONNECT_TIMEOUT).await;
        if is_healthy {
            if was_unhealthy {
                info!("global-egress ({DEFAULT_EGRESS_ADDR}) has recovered and is listening");
                was_unhealthy = false;
            }
            consecutive_failures = 0;
            continue;
        }

        consecutive_failures += 1;
        was_unhealthy = true;

        let backoff = if consecutive_failures >= 10 {
            Duration::from_secs(60)
        } else if consecutive_failures >= 5 {
            Duration::from_secs(30)
        } else {
            MIN_RESTART_INTERVAL
        };

        let now = Instant::now();
        if now.duration_since(last_recovery_attempt) < backoff {
            continue;
        }

        last_recovery_attempt = now;
        warn!(
            failures = consecutive_failures,
            backoff_secs = backoff.as_secs(),
            "global-egress ({DEFAULT_EGRESS_ADDR}) is unreachable; attempting auto-recovery"
        );

        attempt_recovery().await;
    }
}

pub fn is_supervision_needed(proxy_runtime: &ProxyRuntime) -> bool {
    let check_url = |url: &str| -> bool {
        let u = url.trim().to_ascii_lowercase();
        u.contains("127.0.0.1:3128") || u.contains("localhost:3128")
    };

    if check_url(&proxy_runtime.global_proxy_url) {
        return true;
    }

    for policy in proxy_runtime.providers.values() {
        if policy.enabled {
            if check_url(&policy.url) {
                return true;
            }
            if policy.url.trim().is_empty() && check_url(&proxy_runtime.global_proxy_url) {
                return true;
            }
        }
    }

    #[cfg(target_os = "macos")]
    if let Ok(home) = std::env::var("HOME") {
        let p = Path::new(&home).join("Library/LaunchAgents/ai.indo.global-egress.plist");
        if p.is_file() {
            return true;
        }
    }

    false
}

pub async fn check_tcp_port(addr: &str, timeout_dur: Duration) -> bool {
    matches!(timeout(timeout_dur, TcpStream::connect(addr)).await, Ok(Ok(_)))
}

pub async fn attempt_recovery() {
    #[cfg(target_os = "macos")]
    {
        recover_macos().await;
    }
    #[cfg(not(target_os = "macos"))]
    {
        recover_fallback().await;
    }
}

#[cfg(target_os = "macos")]
async fn recover_macos() {
    let uid = get_current_uid().await.unwrap_or_else(|| "501".to_string());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/indo".to_string());
    let home_path = Path::new(&home);

    let egress_plist = home_path.join("Library/LaunchAgents/ai.indo.global-egress.plist");
    let watchdog_plist = home_path.join("Library/LaunchAgents/ai.indo.global-egress-watchdog.plist");

    let target_service = format!("gui/{}/ai.indo.global-egress", uid);
    let check_status = tokio::process::Command::new("launchctl")
        .args(["print", &target_service])
        .output()
        .await;

    let is_loaded = check_status.map(|o| o.status.success()).unwrap_or(false);

    if is_loaded {
        info!("global-egress is registered in launchctl; kickstarting service");
        let _ = tokio::process::Command::new("launchctl")
            .args(["kickstart", "-k", &target_service])
            .status()
            .await;
    } else if egress_plist.exists() {
        warn!("global-egress was unbootstrapped; re-enabling and bootstrapping LaunchAgent");
        let _ = tokio::process::Command::new("launchctl")
            .args(["enable", &target_service])
            .status()
            .await;
        if let Some(plist_str) = egress_plist.to_str() {
            let _ = tokio::process::Command::new("launchctl")
                .args(["bootstrap", &format!("gui/{}", uid), plist_str])
                .status()
                .await;
        }

        if watchdog_plist.exists() {
            let watchdog_service = format!("gui/{}/ai.indo.global-egress-watchdog", uid);
            let _ = tokio::process::Command::new("launchctl")
                .args(["enable", &watchdog_service])
                .status()
                .await;
            if let Some(w_plist_str) = watchdog_plist.to_str() {
                let _ = tokio::process::Command::new("launchctl")
                    .args(["bootstrap", &format!("gui/{}", uid), w_plist_str])
                    .status()
                    .await;
            }
        }
    } else {
        recover_fallback().await;
    }
}

#[cfg(target_os = "macos")]
async fn get_current_uid() -> Option<String> {
    let out = tokio::process::Command::new("id")
        .arg("-u")
        .output()
        .await
        .ok()?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

async fn recover_fallback() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/Users/indo".to_string());
    let bin_path = PathBuf::from(&home).join(".local/bin/global-egress");
    let config_path = PathBuf::from(&home).join(".config/global-egress/config.yaml");

    if bin_path.exists() && config_path.exists() {
        info!("launching fallback global-egress process");
        let _ = tokio::process::Command::new("pkill")
            .args(["-f", "global-egress serve"])
            .status()
            .await;

        let _ = tokio::process::Command::new(bin_path)
            .args(["serve", "-config", config_path.to_str().unwrap_or_default()])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use crate::management::settings::ProviderProxyPolicy;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_check_tcp_port_live() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        assert!(check_tcp_port(&addr, Duration::from_millis(500)).await);
        drop(listener);
        assert!(!check_tcp_port(&addr, Duration::from_millis(100)).await);
    }

    #[test]
    fn test_is_supervision_needed_detection() {
        let mut providers = BTreeMap::new();
        providers.insert(
            "cline".to_string(),
            ProviderProxyPolicy {
                enabled: true,
                url: "".to_string(),
                sticky: true,
                ttl_secs: 21600,
            },
        );

        let runtime = ProxyRuntime {
            global_proxy_url: "http://127.0.0.1:3128".to_string(),
            providers,
        };
        assert!(is_supervision_needed(&runtime));

        let disabled_runtime = ProxyRuntime {
            global_proxy_url: "http://example.com:8080".to_string(),
            providers: BTreeMap::new(),
        };
        let check_url = |url: &str| -> bool {
            let u = url.trim().to_ascii_lowercase();
            u.contains("127.0.0.1:3128") || u.contains("localhost:3128")
        };
        assert!(!check_url(&disabled_runtime.global_proxy_url));
    }
}

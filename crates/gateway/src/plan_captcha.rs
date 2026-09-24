//! Automatic resolution of the zcode plan gateway's Aliyun WAF challenge.
//!
//! On a challenge (biz 3007 in the body or the `x-aliyun-captcha-verify-param`
//! response header) the relay reads the captcha scene from the plan gateway's
//! public client config, mints a verify param with the vendored traceless
//! solver sidecar (opencodex PR #4437), and replays the challenged request
//! ONCE with `X-Aliyun-Captcha-Verify-Param` / `X-Aliyun-Captcha-Verify-Region`.
//! A replay that is challenged again, and biz 3012, are hard blocks — they
//! surface as upstream_error without further retries.
//!
//! The solver is a one-shot process (`captcha-solver/index.ts` compiled with
//! `bun build --compile`): process isolation replaces the reference's
//! worker-thread host. Verify params are single-use, so solves are serialized
//! through the per-state gate — no global static state (gateway AGENTS.md).

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::AsyncBufReadExt;

const CONFIG_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Extra kill deadline over the solve budget — covers sidecar startup and the
/// host-side deadline the reference implementation adds around its worker.
const SOLVE_KILL_GRACE_MS: u64 = 20_000;

/// Everything the challenge handler needs: the pieces to replay the challenged
/// request and the state-owned overrides/gate (tests inject their own values).
pub struct ReplayPieces<'a> {
    pub client: &'a reqwest::Client,
    pub url: &'a str,
    pub headers: Vec<(String, String)>,
    pub content_type: Option<String>,
    pub accept: Option<String>,
    pub body: bytes::Bytes,
    pub config_url: Option<String>,
    pub solver_bin: Option<PathBuf>,
    pub solve_gate: &'a tokio::sync::Mutex<()>,
}

/// Resolver order mirrors the gateway binary's own: state-level override
/// (config/env-resolved at startup), then the solver staged next to the
/// running executable by setup-gateway.sh.
fn solver_binary_path(state_override: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = state_override {
        if path.is_file() {
            return Some(path.to_path_buf());
        }
    }
    let exe = std::env::current_exe().ok()?;
    let sibling = exe.parent()?.join("mahoquot-captcha-solver");
    sibling.is_file().then_some(sibling)
}

/// Read the captcha scene from the plan gateway's public client config.
pub async fn fetch_captcha_scene(
    client: &reqwest::Client,
    origin: &str,
    config_url_override: Option<&str>,
) -> Result<mahoquot_providers::zcode::PlanCaptchaScene, String> {
    let default_url = mahoquot_providers::zcode::captcha_config_url(
        origin,
        mahoquot_providers::zcode::ZCODE_APP_VERSION,
        &format!(
            "{}-{}",
            crate::account::zcode_node_platform(),
            crate::account::zcode_node_arch()
        ),
    );
    let url: String = config_url_override
        .filter(|u| !u.trim().is_empty())
        .map(str::to_string)
        .unwrap_or(default_url);
    let response = tokio::time::timeout(CONFIG_FETCH_TIMEOUT, client.get(url.as_str()).send())
        .await
        .map_err(|_| "captcha config fetch timed out".to_string())?
        .map_err(|e| format!("captcha config fetch failed: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "captcha config fetch failed: status {}",
            response.status()
        ));
    }
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("captcha config body unreadable: {e}"))?;
    mahoquot_providers::zcode::parse_captcha_scene(&body)
        .ok_or_else(|| "captcha scene unavailable or disabled in client config".to_string())
}

/// Mint one verify param via the solver sidecar. The sidecar reports its own
/// outcome as JSON on stdout; a hung run is killed at the deadline and the
/// solve fails, never blocking the caller.
pub async fn solve(
    scene: &mahoquot_providers::zcode::PlanCaptchaScene,
    timeout_ms: u64,
    solve_gate: &tokio::sync::Mutex<()>,
    solver_bin_override: Option<&Path>,
) -> Result<String, String> {
    let binary = solver_binary_path(solver_bin_override).ok_or_else(|| {
        "captcha solver binary not found (set MAHOQUOT_CAPTCHA_SOLVER_BIN or stage mahoquot-captcha-solver next to the gateway binary)".to_string()
    })?;
    // Verify params are single-use; concurrent challengers must not share one.
    let _gate = solve_gate.lock().await;

    let request = serde_json::json!({
        "scene": scene.scene_id,
        "region": scene.region,
        "prefix": scene.prefix,
        "timeoutMs": timeout_ms,
    });
    let mut child = tokio::process::Command::new(&binary)
        .arg(request.to_string())
        // The sidecar must never share the gateway's stdin: inheriting it lets a
        // solver that reads stdin block until the gateway's own input closes,
        // which burns the whole solve deadline and can steal bytes meant for the
        // gateway. A null stdin reaches EOF immediately.
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("captcha solver spawn failed: {e}"))?;
    let mut stdout = tokio::io::BufReader::new(
        child
            .stdout
            .take()
            .ok_or_else(|| "captcha solver stdout unavailable".to_string())?,
    );
    let mut line = String::new();
    // Solve request travels as ONE argv item (no shell involved). The sidecar
    // contract deliberately avoids a stdin pipe: tokio's ChildStdin drop does
    // not deliver EOF to the child on macOS, deadlocking read-to-EOF sidecars.
    let run = async {
        let read = stdout
            .read_line(&mut line)
            .await
            .map_err(|e| format!("captcha solver stdout read failed: {e}"))?;
        if read == 0 {
            return Err("captcha solver exited without output".to_string());
        }
        Ok(())
    };
    let deadline = Duration::from_millis(timeout_ms + SOLVE_KILL_GRACE_MS);
    match tokio::time::timeout(deadline, run).await {
        Ok(Ok(())) => {}
        Ok(Err(failure)) => {
            let _ = child.kill().await;
            return Err(failure);
        }
        Err(_elapsed) => {
            let _ = child.kill().await;
            return Err(format!("captcha solver timed out after {timeout_ms}ms"));
        }
    }
    let _ = child.kill().await;
    if line.trim().is_empty() {
        return Err("captcha solver produced no output (crashed?)".to_string());
    }
    let parsed: serde_json::Value = serde_json::from_str(line.trim())
        .map_err(|e| format!("captcha solver output unreadable: {e}"))?;
    match (
        parsed.get("ok").and_then(serde_json::Value::as_bool),
        parsed.get("param").and_then(serde_json::Value::as_str),
    ) {
        (Some(true), Some(param)) if !param.trim().is_empty() => Ok(param.trim().to_string()),
        (Some(true), _) => Err("captcha solver returned an empty verify param".to_string()),
        _ => Err(format!(
            "captcha solve failed: {}",
            parsed
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown solver error")
        )),
    }
}

/// Full challenge posture for one challenged request: scene config, solve,
/// single replay with verify headers.
pub async fn resolve_challenge(pieces: &ReplayPieces<'_>) -> Result<reqwest::Response, String> {
    let scene = fetch_captcha_scene(
        pieces.client,
        mahoquot_providers::zcode::ZCODE_PLAN_ORIGIN,
        pieces.config_url.as_deref(),
    )
    .await?;
    eprintln!(
        "[zcode-plan] WAF challenge (biz 3007) — solving captcha scene {} (region {})",
        scene.scene_id, scene.region
    );
    let param = solve(
        &scene,
        mahoquot_providers::zcode::ZCODE_CAPTCHA_SOLVE_TIMEOUT_MS,
        pieces.solve_gate,
        pieces.solver_bin.as_deref(),
    )
    .await?;
    eprintln!(
        "[zcode-plan] captcha solve succeeded ({} bytes) — replaying once",
        param.len()
    );
    let mut request = pieces.client.post(pieces.url);
    for (name, value) in &pieces.headers {
        request = request.header(name, value);
    }
    if let Some(accept) = pieces.accept.as_deref() {
        request = request.header(reqwest::header::ACCEPT, accept);
    }
    if let Some(content_type) = pieces.content_type.as_deref() {
        request = request.header(reqwest::header::CONTENT_TYPE, content_type);
    }
    request
        .header(
            mahoquot_providers::zcode::ZCODE_CAPTCHA_VERIFY_PARAM_HEADER,
            &param,
        )
        .header(
            mahoquot_providers::zcode::ZCODE_CAPTCHA_VERIFY_REGION_HEADER,
            &scene.region,
        )
        .body(pieces.body.clone())
        .send()
        .await
        .map_err(|e| format!("captcha replay request failed: {e}"))
}

#[cfg(test)]
mod sidecar_handshake_tests {
    use super::*;

    /// The one-shot sidecar handshake must terminate: spawn via argv, read one
    /// JSON result line, kill at the deadline. Guards the argv-only contract —
    /// a stdin pipe deadlocks here (tokio's ChildStdin drop does not deliver
    /// EOF to the child on macOS).
    #[tokio::test]
    async fn handshake_terminates() {
        let dir = std::env::temp_dir().join(format!("scratch-solver-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("started.marker");
        // The production sidecar contract: request via argv, no stdin read.
        // The marker proves exec ran; the printf proves the output handshake.
        let script = dir.join("solver.sh");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho started > {}\ncat > /dev/null\nprintf '%s\\n' '{{\"ok\":true,\"param\":\"p\"}}'\n",
                marker.display()
            ),
        )
        .unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
        }
        let scene = mahoquot_providers::zcode::PlanCaptchaScene {
            scene_id: "s".into(),
            prefix: "p".into(),
            region: "sgp".into(),
        };
        let gate = tokio::sync::Mutex::new(());
        let started = std::time::Instant::now();
        let result = solve(&scene, 3000, &gate, Some(&script)).await;
        let exec_ran = marker.exists();
        println!(
            "SCRATCH ({:?}) exec_ran={exec_ran}: {result:?}",
            started.elapsed()
        );
        assert!(exec_ran, "the solver script never executed");
        assert!(result.is_ok(), "handshake failed: {result:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context};
use serde::Deserialize;
use serde_json::{json, Map, Value};

use crate::management::settings::OmoProfile;
use crate::state::AppState;

const API: &str = "https://api.cline.bot/api/v1";

#[derive(Deserialize)]
struct Recommendations {
    free: Vec<FreeModel>,
}

#[derive(Deserialize)]
struct FreeModel {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct Metadata {
    data: Vec<Value>,
}

fn metadata_for<'a>(id: &str, catalog: &'a [Value]) -> Option<&'a Value> {
    if let Some(exact) = catalog.iter().find(|m| m["id"].as_str() == Some(id)) {
        return Some(exact);
    }
    let slug = id.rsplit('/').next()?.trim_end_matches("-free").trim_end_matches(":free");
    let matches: Vec<_> = catalog.iter().filter(|m| m["id"].as_str().is_some_and(|candidate|
        candidate.rsplit('/').next().is_some_and(|part| part.trim_end_matches(":free") == slug))).collect();
    if let Some(free) = matches.iter().find(|m| m["id"].as_str().is_some_and(|id| id.ends_with(":free"))) {
        return Some(free);
    }
    if matches.len() == 1 { return matches.first().copied(); }
    let prefix = format!("{}-", id.trim_end_matches("-free"));
    let mut matches = catalog.iter().filter(|m| m["id"].as_str().is_some_and(|id| id.starts_with(&prefix)));
    let first = matches.next()?;
    if matches.next().is_none() { Some(first) } else { None }
}

pub async fn fetch_free_models(
    client: &reqwest::Client,
    base: &str,
) -> anyhow::Result<Vec<Map<String, Value>>> {
    let catalog: Recommendations = client
        .get(format!("{base}/ai/cline/recommended-models"))
        .timeout(Duration::from_secs(30))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let metadata: Metadata = client.get(format!("{base}/ai/cline/models"))
        .timeout(Duration::from_secs(30)).send().await?.error_for_status()?.json().await?;
    let mut ids = BTreeSet::new();
    let mut models = Vec::with_capacity(catalog.free.len());
    for entry in catalog.free {
        if entry.id.trim().is_empty() || !ids.insert(entry.id.clone()) {
            bail!("invalid Cline free catalog IDs");
        }
        let mut model = Map::new();
        if let Some(meta) = metadata_for(&entry.id, &metadata.data) {
            if let Some(context) = meta["context_length"].as_u64().filter(|n| *n > 0) {
                model.insert("contextWindow".into(), json!(context));
            }
            if let Some(max) = meta["top_provider"]["max_completion_tokens"].as_u64().filter(|n| *n > 0) {
                model.insert("maxTokens".into(), json!(max));
            }
            if let Some(input) = meta["architecture"]["input_modalities"].as_array() {
                model.insert("input".into(), json!(input.iter().filter(|v| matches!(v.as_str(), Some("text" | "image"))).collect::<Vec<_>>()));
            }
            if let Some(params) = meta["supported_parameters"].as_array() {
                model.insert("reasoning".into(), json!(params.iter().any(|p| matches!(p.as_str(), Some("reasoning" | "include_reasoning")))));
            }
        }
        model.insert("cost".into(), json!({"input":0,"output":0,"cacheRead":0,"cacheWrite":0}));
        model.insert("id".into(), Value::String(entry.id));
        model.insert("name".into(), Value::String(entry.name));
        models.push(model);
    }
    models.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    Ok(models)
}

fn merge_profile(
    profile: &mut OmoProfile,
    models: &[Map<String, Value>],
    previous: &BTreeSet<String>,
) -> bool {
    let before = profile.clone();
    let ids: BTreeSet<&str> = models
        .iter()
        .filter_map(|m| m.get("id")?.as_str())
        .collect();
    let mut removed = previous.clone();
    profile.models.retain(|model| {
        let Some(id) = model.get("id").and_then(Value::as_str) else {
            return true;
        };
        if id.starts_with("cline-free/") || previous.contains(id) || ids.contains(id) {
            removed.insert(id.to_string());
            false
        } else {
            true
        }
    });
    profile.models.extend_from_slice(models);
    profile.remove.retain(|id| !ids.contains(id.as_str()));
    for id in removed {
        if !ids.contains(id.as_str()) && !profile.remove.contains(&id) {
            profile.remove.push(id);
        }
    }
    if profile.models == before.models && profile.remove == before.remove {
        return false;
    }
    profile.revision = profile.revision.saturating_add(1);
    profile.updated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    true
}

pub async fn refresh(state: Arc<AppState>, base: &str) -> anyhow::Result<()> {
    let members: Vec<_> = state
        .pool
        .load()
        .members
        .iter()
        .filter(|m| m.provider_name() == "cline")
        .cloned()
        .collect();
    if members.is_empty() {
        return Ok(());
    }
    let models = fetch_free_models(&state.http_client, base).await?;
    let ids: Vec<_> = models
        .iter()
        .filter_map(|m| m.get("id")?.as_str())
        .map(str::to_string)
        .collect();
    let mut previous = BTreeSet::new();
    let mut changed = false;
    for member in members {
        let guard = Arc::clone(&member.refresh_lock).lock_owned().await;
        let ids = ids.clone();
        let (old, updated) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
            let _guard = guard;
            let mut value: Value = serde_json::from_slice(&std::fs::read(&member.file_path)?)?;
            let old: BTreeSet<String> = value
                .get("cline_free_models")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            let existing = value
                .get("models")
                .and_then(Value::as_array)
                .context("Cline models must be an array")?;
            let mut next: Vec<Value> = existing
                .iter()
                .filter(|v| {
                    v.as_str().is_some_and(|id| {
                        !id.starts_with("cline-free/")
                            && !old.contains(id)
                            && !ids.iter().any(|new| new == id)
                    })
                })
                .cloned()
                .collect();
            next.extend(ids.iter().map(|id| json!(id)));
            let managed: BTreeSet<_> = old.iter().cloned().chain(ids.iter().cloned()).collect();
            let updated =
                value["models"] != json!(next) || value["cline_free_models"] != json!(managed);
            if updated {
                value["models"] = json!(next);
                value["cline_free_models"] = json!(managed);
                mahoquot_providers::credential_file::write_credential_atomically(
                    &member.file_path,
                    &serde_json::to_vec_pretty(&value)?,
                )?;
            }
            Ok((old, updated))
        })
        .await??;
        previous.extend(old);
        changed |= updated;
    }
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        if changed {
            state.rescan_pool()?;
        }
        match state.settings.try_mutate(|settings| {
            let mut profile = settings.omo_profile.clone().unwrap_or_default();
            if !merge_profile(&mut profile, &models, &previous) {
                return Err(());
            }
            settings.omo_profile = Some(profile);
            Ok(())
        }) {
            Ok(result) => {
                result?;
            }
            Err(()) => {}
        }
        Ok(())
    })
    .await??;
    Ok(())
}

pub struct ClineCatalogHandle {
    stop: Arc<tokio::sync::Notify>,
    join: tokio::task::JoinHandle<()>,
}

impl ClineCatalogHandle {
    pub async fn shutdown(self) {
        self.stop.notify_one();
        if let Err(error) = self.join.await {
            tracing::warn!(%error, "Cline catalog worker failed during shutdown");
        }
    }
}

pub fn spawn(state: Arc<AppState>) -> ClineCatalogHandle {
    let stop = Arc::new(tokio::sync::Notify::new());
    let signal = Arc::clone(&stop);
    let join = tokio::spawn(async move {
        let mut timer = tokio::time::interval(Duration::from_secs(300));
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = signal.notified() => break,
                _ = timer.tick() => {}
            }
            if let Err(error) = refresh(Arc::clone(&state), API).await {
                tracing::warn!(%error, "Cline free catalog refresh failed; retaining last successful catalog");
            }
        }
    });
    ClineCatalogHandle { stop, join }
}

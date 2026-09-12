use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use mahoquot_registry::{
    CatalogSource, ModelDescriptor, ModelId, ProviderBinding, ProviderId, ProviderPolicy,
    RegistryError, RegistrySnapshot,
};

use crate::account::{AccountMember, ProviderKind};
use crate::models_route::ModelEntry;

/// Combined immutable composition holding account membership, effective model list,
/// snapshot-scoped permissions, and the catalog registry snapshot at a single monotonic generation.
#[derive(Clone)]
pub struct PoolSnapshot {
    pub generation: u64,
    pub members: Vec<Arc<AccountMember>>,
    pub models: Vec<ModelEntry>,
    pub registry: Arc<RegistrySnapshot>,
    pub permissions: Arc<std::collections::HashMap<String, AccountSnapshotPermissions>>,
}

impl std::fmt::Debug for PoolSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let member_ids: Vec<&str> = self.members.iter().map(|m| m.id.as_str()).collect();
        f.debug_struct("PoolSnapshot")
            .field("generation", &self.generation)
            .field("members", &member_ids)
            .field("models", &self.models)
            .field("registry", &self.registry)
            .field("permissions", &self.permissions)
            .finish()
    }
}

/// Snapshot-scoped immutable permissions and credential identity captured at a single generation.
/// This prevents in-flight requests or older snapshot holders from observing mutations in shared
/// member state across credential rotations or catalog refreshes.
#[derive(Clone, PartialEq)]
pub struct AccountSnapshotPermissions {
    pub identity_slug: String,
    pub provider_kind: ProviderKind,
    pub disabled: bool,
    pub access_token: String,
    pub effective_base_url: String,
    pub devin_models: Option<Vec<String>>,
    pub devin_discovered: Option<Vec<crate::devin_catalog::DiscoveredDevinModel>>,
    pub unsupported_models: Vec<String>,
}

impl std::fmt::Debug for AccountSnapshotPermissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountSnapshotPermissions")
            .field("identity_slug", &self.identity_slug)
            .field("provider_kind", &self.provider_kind)
            .field("disabled", &self.disabled)
            .field("access_token", &"[REDACTED]")
            .field("effective_base_url", &self.effective_base_url)
            .field("devin_models", &self.devin_models)
            .field("devin_discovered", &self.devin_discovered)
            .field("unsupported_models", &self.unsupported_models)
            .finish()
    }
}

impl AccountSnapshotPermissions {
    pub fn supports_devin_model(&self, requested: &str, canonical: &str, upstream: &str) -> bool {
        if self.provider_kind != ProviderKind::Devin || self.disabled {
            return false;
        }
        if self
            .unsupported_models
            .iter()
            .any(|m| m == requested || m == canonical || m == upstream)
        {
            return false;
        }
        match &self.devin_models {
            Some(models) => models.iter().any(|m| {
                m == requested
                    || m == canonical
                    || m == upstream
                    || m.strip_prefix("devin/").unwrap_or(m) == upstream
            }),
            None => false,
        }
    }

    pub fn devin_model_supports_vision(&self, model: &str) -> bool {
        if self.provider_kind != ProviderKind::Devin || self.disabled {
            return false;
        }
        let Some(ref discovered) = self.devin_discovered else {
            return false;
        };
        discovered.iter().any(|m| {
            (m.public_id == model || m.model_uid == model) && m.supports_images
        })
    }
}

pub type RuntimeComposition = PoolSnapshot;

impl PoolSnapshot {
    pub fn new(
        generation: u64,
        members: Vec<Arc<AccountMember>>,
        models: Vec<ModelEntry>,
        registry: Arc<RegistrySnapshot>,
    ) -> Self {
        let mut permissions = std::collections::HashMap::with_capacity(members.len());
        for m in &members {
            let access_token = m.access_token();
            let effective_base_url = m.effective_base_url();
            let disabled = m.is_manually_disabled();
            let unsupported = m
                .unsupported_models
                .read()
                .unwrap_or_else(|p| p.into_inner())
                .clone();
            let devin_models = m.devin_models();
            let devin_discovered = m.devin_discovered_models();
            permissions.insert(
                m.id.clone(),
                AccountSnapshotPermissions {
                    identity_slug: m.id.clone(),
                    provider_kind: m.kind(),
                    disabled,
                    access_token,
                    effective_base_url,
                    devin_models,
                    devin_discovered,
                    unsupported_models: unsupported,
                },
            );
        }

        Self {
            generation,
            members,
            models,
            registry,
            permissions: Arc::new(permissions),
        }
    }

    #[inline]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    #[inline]
    pub fn members(&self) -> &[Arc<AccountMember>] {
        &self.members
    }

    #[inline]
    pub fn models(&self) -> &[ModelEntry] {
        &self.models
    }

    #[inline]
    pub fn registry(&self) -> &Arc<RegistrySnapshot> {
        &self.registry
    }

    #[inline]
    pub fn permissions(&self) -> &Arc<std::collections::HashMap<String, AccountSnapshotPermissions>> {
        &self.permissions
    }

    pub fn account_permissions(&self, id: &str) -> Option<&AccountSnapshotPermissions> {
        self.permissions.get(id)
    }

    pub fn devin_account_models(&self, id: &str) -> Option<&[String]> {
        self.permissions.get(id).and_then(|p| p.devin_models.as_deref())
    }

    pub fn is_devin_model_eligible(&self, id: &str, model: &str) -> bool {
        let canonical = if model.starts_with("devin/") {
            model.to_string()
        } else {
            format!("devin/{model}")
        };
        let upstream = model.strip_prefix("devin/").unwrap_or(model);
        self.permissions
            .get(id)
            .is_some_and(|p| p.supports_devin_model(model, &canonical, upstream))
    }

    pub fn devin_model_supports_vision(&self, id: &str, model: &str) -> bool {
        self.permissions
            .get(id)
            .is_some_and(|p| p.devin_model_supports_vision(model))
    }

    pub fn devin_credential_revision(&self, id: &str) -> Option<&str> {
        self.permissions.get(id).map(|p| p.access_token.as_str())
    }

    pub fn devin_effective_base_url(&self, id: &str) -> Option<&str> {
        self.permissions.get(id).map(|p| p.effective_base_url.as_str())
    }

    pub fn find_member(&self, id: &str) -> Option<Arc<AccountMember>> {
        self.members.iter().find(|m| m.id == id).cloned()
    }

    pub fn routable_accounts_for_model(&self, model: &str) -> Vec<Arc<AccountMember>> {
        let (prefix, _) = crate::relay::parse_model_prefix(model);
        let Ok(resolved) = crate::relay::resolve_model(self, model) else {
            return Vec::new();
        };
        self.members
            .iter()
            .filter(|m| {
                match prefix {
                    Some(crate::relay::ModelPrefix::Anthropic)
                        if m.kind() != ProviderKind::Claude || m.is_nekos_relay() =>
                    {
                        return false;
                    }
                    Some(crate::relay::ModelPrefix::Nekos)
                        if m.kind() != ProviderKind::Claude || !m.is_nekos_relay() =>
                    {
                        return false;
                    }
                    _ => {}
                }
                resolved.eligible_bindings.iter().any(|binding| {
                    if ProviderId::canonical(m.provider_name()).ok().as_ref()
                        != Some(&binding.provider_id)
                    {
                        return false;
                    }
                    let canonical = resolved.canonical_id.as_str();
                    let upstream = binding.effective_upstream_id(&resolved.canonical_id);

                    let unsupported = m.unsupported_models.read().unwrap_or_else(|p| p.into_inner());
                    if unsupported.iter().any(|id| id == model || id == canonical || id == upstream) {
                        return false;
                    }

                    if let Some(perm) = self.permissions.get(&m.id) {
                        if perm.disabled {
                            return false;
                        }
                        if perm.provider_kind == ProviderKind::Devin {
                            return perm.supports_devin_model(model, canonical, upstream);
                        }
                    } else if m.kind() == ProviderKind::Devin {
                        return m.supports_devin_model(model, canonical, upstream);
                    }

                    m.generic_models().is_none_or(|(_, models)| {
                        models.is_empty()
                            || models.iter().any(|id| id == model || id == canonical || id == upstream)
                    })
                })
            })
            .cloned()
            .collect()
    }
}

fn registry_with_account_contributions(
    members: &[Arc<AccountMember>],
    registry: &RegistrySnapshot,
) -> Result<RegistrySnapshot, RegistryError> {
    let mut effective = registry.clone();
    for member in members {
        let Some((provider_name, models)) = member.generic_models() else {
            continue;
        };
        // An empty generic list historically meant "open", but the unified
        // registry only permits explicit Open policy. Account-declared IDs are
        // authoritative discovered bindings; empty declarations contribute no
        // catch-all binding.
        if models.is_empty() {
            continue;
        }
        let provider_id = ProviderId::canonical(provider_name)?;
        match effective.providers.get(&provider_id) {
            Some(ProviderPolicy::Closed) => {
                for model in &models {
                    let model_id = ModelId::new(model)?;
                    let is_authorized = effective
                        .models
                        .get(&model_id)
                        .is_some_and(|desc| desc.bindings.contains_key(&provider_id));
                    if !is_authorized {
                        return Err(RegistryError::UnauthorizedContribution {
                            provider_id,
                            policy: ProviderPolicy::Closed,
                            model_id,
                        });
                    }
                }
                continue;
            }
            Some(_) => {}
            None => {
                effective
                    .providers
                    .insert(provider_id.clone(), ProviderPolicy::Discovered);
            }
        }
        for model in &models {
            let model_id = ModelId::new(model)?;
            let descriptor = effective
                .models
                .entry(model_id.clone())
                .or_insert_with(|| ModelDescriptor::new(model_id.clone(), provider_id.as_str()));
            descriptor
                .bindings
                .entry(provider_id.clone())
                .or_insert_with(|| {
                    ProviderBinding::new(
                        provider_id.clone(),
                        ProviderPolicy::Discovered,
                        CatalogSource::Discovered,
                    )
                });
        }
    }
    for member in members {
        if member.kind() == ProviderKind::Devin && !member.is_manually_disabled() {
            if let Some(models) = member.devin_discovered_models() {
                if !models.is_empty() {
                    let provider_id = ProviderId::devin();
                    if !effective.providers.contains_key(&provider_id) {
                        effective
                            .providers
                            .insert(provider_id.clone(), ProviderPolicy::Discovered);
                    }
                    for model in &models {
                        let model_id = ModelId::new(&model.public_id)?;
                        let descriptor = effective
                            .models
                            .entry(model_id.clone())
                            .or_insert_with(|| {
                                let mut desc = ModelDescriptor::new(model_id.clone(), "devin");
                                desc.capabilities.insert(mahoquot_registry::ModelCapability::Chat);
                                desc.capabilities.insert(mahoquot_registry::ModelCapability::Tools);
                                desc
                            });
                        descriptor
                            .bindings
                            .entry(provider_id.clone())
                            .or_insert_with(|| {
                                ProviderBinding::new(
                                    provider_id.clone(),
                                    ProviderPolicy::Discovered,
                                    CatalogSource::Discovered,
                                )
                                .with_upstream_id(&model.model_uid)
                                .with_capabilities([
                                    mahoquot_registry::ModelCapability::Chat,
                                    mahoquot_registry::ModelCapability::Tools,
                                ])
                            });
                    }
                }
            }
        }
    }
    effective.validate()?;
    Ok(effective)
}

/// Computes a candidate runtime composition from candidate accounts and registry snapshot.
/// Recomputes the candidate effective model list and ensures domain invariants hold.
pub fn compute_candidate_composition(
    generation: u64,
    members: Vec<Arc<AccountMember>>,
    registry: Arc<RegistrySnapshot>,
    models_env: Option<&str>,
) -> Result<RuntimeComposition, RegistryError> {
    let mut effective_registry = registry_with_account_contributions(&members, &registry)?;

    if let Some(raw) = models_env {
        let has_active_codex = members
            .iter()
            .any(|m| m.kind() == ProviderKind::Codex && !m.is_manually_disabled());
        if has_active_codex {
            for id_str in crate::models_route::model_ids_from_env(Some(raw)) {
                let model_id = ModelId::new(&id_str)?;
                let descriptor = effective_registry
                    .models
                    .entry(model_id.clone())
                    .or_insert_with(|| ModelDescriptor::new(model_id.clone(), "openai"));
                descriptor
                    .bindings
                    .entry(ProviderId::codex())
                    .or_insert_with(|| {
                        ProviderBinding::new(
                            ProviderId::codex(),
                            ProviderPolicy::Open,
                            CatalogSource::LocalOverride,
                        )
                        .with_capabilities([mahoquot_registry::ModelCapability::Chat])
                    });
            }
        }
    }

    effective_registry.validate()?;
    let registry = Arc::new(effective_registry);

    let models = crate::models_route::project_model_entries(&registry, &members);
    let models = crate::models_route::expand_prefixed_models(models, &members);

    Ok(RuntimeComposition::new(
        generation, members, models, registry,
    ))
}

/// Serializes refresh triggers and coalesces overlapping refresh requests.
/// Readers on the relay path never acquire this coordinator lock.
pub struct RefreshCoordinator {
    mutex: std::sync::Mutex<()>,
    request_seq: AtomicU64,
    completed_seq: AtomicU64,
    condvar: std::sync::Condvar,
}

impl Default for RefreshCoordinator {
    fn default() -> Self {
        Self {
            mutex: std::sync::Mutex::new(()),
            request_seq: AtomicU64::new(0),
            completed_seq: AtomicU64::new(0),
            condvar: std::sync::Condvar::new(),
        }
    }
}

impl RefreshCoordinator {
    pub fn coordinate<F>(&self, action: F) -> Result<u64, anyhow::Error>
    where
        F: FnOnce() -> Result<u64, anyhow::Error>,
    {
        let my_req = self.request_seq.fetch_add(1, Ordering::SeqCst) + 1;

        let _lock = self.mutex.lock().unwrap_or_else(|p| p.into_inner());

        let completed = self.completed_seq.load(Ordering::SeqCst);
        if completed >= my_req {
            return Ok(completed);
        }

        let target = self.request_seq.load(Ordering::SeqCst);
        match action() {
            Ok(new_gen) => {
                self.completed_seq.store(target, Ordering::SeqCst);
                self.condvar.notify_all();
                Ok(new_gen)
            }
            Err(e) => {
                self.condvar.notify_all();
                Err(e)
            }
        }
    }

    /// Executes a parameterized mutation under the exclusive coordinator lock,
    /// ensuring it is never dropped by sequence coalescing and always executes sequentially.
    pub fn exclusive<F, R>(&self, action: F) -> Result<R, anyhow::Error>
    where
        F: FnOnce() -> Result<R, anyhow::Error>,
    {
        let _lock = self.mutex.lock().unwrap_or_else(|p| p.into_inner());
        action()
    }
}

/// Unified runtime container that pairs account pool membership and model registry state.
/// Holds a single ArcSwap pointer so readers never observe split-brain half-states.
pub struct UnifiedRuntimeState {
    pool: Arc<arc_swap::ArcSwap<RuntimeComposition>>,
    generation_seq: AtomicU64,
    coordinator: Arc<RefreshCoordinator>,
    models_env: Option<String>,
}

pub type RuntimeState = UnifiedRuntimeState;

impl UnifiedRuntimeState {
    pub fn new(initial: RuntimeComposition, models_env: Option<String>) -> Self {
        let generation_seq = AtomicU64::new(initial.generation);
        let pool = Arc::new(arc_swap::ArcSwap::from_pointee(initial));
        Self {
            pool,
            generation_seq,
            coordinator: Arc::new(RefreshCoordinator::default()),
            models_env,
        }
    }

    #[inline]
    pub fn load(&self) -> arc_swap::Guard<Arc<RuntimeComposition>> {
        self.pool.load()
    }

    #[inline]
    pub fn composition(&self) -> Arc<RuntimeComposition> {
        self.pool.load_full()
    }

    #[inline]
    pub fn pool(&self) -> Arc<arc_swap::ArcSwap<RuntimeComposition>> {
        Arc::clone(&self.pool)
    }

    #[inline]
    pub fn generation(&self) -> u64 {
        self.pool.load().generation
    }

    pub fn publish_candidate(
        &self,
        candidate: RuntimeComposition,
    ) -> Result<Arc<RuntimeComposition>, anyhow::Error> {
        self.coordinator.exclusive(|| {
            candidate.registry.validate()?;
            let arc_candidate = Arc::new(candidate);
            self.generation_seq
                .store(arc_candidate.generation, Ordering::SeqCst);
            self.pool.store(Arc::clone(&arc_candidate));
            Ok(arc_candidate)
        })
    }

    pub fn update_registry(
        &self,
        next_registry: Arc<RegistrySnapshot>,
    ) -> Result<Arc<RuntimeComposition>, anyhow::Error> {
        self.update_registry_with_commit(next_registry, || Ok(()))
    }

    pub(crate) fn update_registry_with_commit(
        &self,
        next_registry: Arc<RegistrySnapshot>,
        commit: impl FnOnce() -> anyhow::Result<()>,
    ) -> Result<Arc<RuntimeComposition>, anyhow::Error> {
        let models_env = self.models_env.clone();
        let pool = Arc::clone(&self.pool);
        let gen_seq = &self.generation_seq;

        self.coordinator.exclusive(|| {
            let next_gen = gen_seq.fetch_add(1, Ordering::SeqCst) + 1;
            let current_members = pool.load().members.clone();
            let candidate = compute_candidate_composition(
                next_gen,
                current_members,
                next_registry,
                models_env.as_deref(),
            )?;
            commit()?;
            let arc_candidate = Arc::new(candidate);
            pool.store(Arc::clone(&arc_candidate));
            Ok(arc_candidate)
        })
    }

    pub fn reload_accounts(
        &self,
        new_members: Vec<Arc<AccountMember>>,
    ) -> Result<Arc<RuntimeComposition>, anyhow::Error> {
        let models_env = self.models_env.clone();
        let pool = Arc::clone(&self.pool);
        let gen_seq = &self.generation_seq;

        self.coordinator.exclusive(|| {
            let next_gen = gen_seq.fetch_add(1, Ordering::SeqCst) + 1;
            let current_registry = Arc::clone(&pool.load().registry);
            let candidate = compute_candidate_composition(
                next_gen,
                new_members,
                current_registry,
                models_env.as_deref(),
            )?;
            let arc_candidate = Arc::new(candidate);
            pool.store(Arc::clone(&arc_candidate));
            Ok(arc_candidate)
        })
    }

    pub fn trigger_coalesced_refresh(&self) -> Result<Arc<RuntimeComposition>, anyhow::Error> {
        let models_env = self.models_env.clone();
        let pool = Arc::clone(&self.pool);
        let gen_seq = &self.generation_seq;

        self.coordinator.coordinate(|| {
            let next_gen = gen_seq.fetch_add(1, Ordering::SeqCst) + 1;
            let current_members = pool.load().members.clone();
            let current_registry = Arc::clone(&pool.load().registry);
            let candidate = compute_candidate_composition(
                next_gen,
                current_members,
                current_registry,
                models_env.as_deref(),
            )?;
            pool.store(Arc::new(candidate));
            Ok(next_gen)
        })?;

        Ok(self.composition())
    }

    pub fn publish_devin_member_catalog(
        &self,
        target_id: &str,
        target_revision: &str,
        new_catalog: Arc<crate::devin_catalog::DevinAccountCatalogState>,
        cache: &crate::devin_catalog::DevinDiscoveryCache,
    ) -> Result<Arc<RuntimeComposition>, crate::devin_catalog::DevinDiscoveryError> {
        self.coordinator
            .exclusive(|| {
                let current_snapshot = self.pool.load();
                let member = current_snapshot
                    .find_member(target_id)
                    .ok_or_else(|| anyhow::Error::new(crate::devin_catalog::DevinDiscoveryError::StalePublication))?;

                if member.kind() != ProviderKind::Devin || member.is_manually_disabled() {
                    return Err(anyhow::Error::new(crate::devin_catalog::DevinDiscoveryError::StalePublication));
                }

                let current_token = member.access_token();
                let current_base_url = member.effective_base_url();
                let current_rev =
                    crate::devin_catalog::compute_credential_revision(&current_token);
                if current_rev != target_revision {
                    return Err(anyhow::Error::new(crate::devin_catalog::DevinDiscoveryError::StalePublication));
                }

                let expected_key = crate::devin_catalog::DevinCacheKey {
                    identity_slug: target_id.to_string(),
                    credential_revision: current_rev,
                    credential_token: current_token,
                    base_url: current_base_url,
                };

                // Full key validation: token, identity, and effective base URL must match
                if new_catalog.key != expected_key {
                    return Err(anyhow::Error::new(crate::devin_catalog::DevinDiscoveryError::StalePublication));
                }

                // Monotonicity / out-of-order rejection:
                // Pre-RPC monotonic request sequencing takes strict precedence over completion timestamps.
                // An older in-flight request finishing late has new_catalog.sequence <= existing.sequence
                // and is immediately rejected as StalePublication.
                if let Some(existing) = member.devin_catalog_state() {
                    if existing.key == expected_key {
                        if new_catalog.sequence > 0 && existing.sequence > 0 {
                            if new_catalog.sequence <= existing.sequence {
                                return Err(anyhow::Error::new(crate::devin_catalog::DevinDiscoveryError::StalePublication));
                            }
                        } else if let (Some(existing_ts), Some(new_ts)) = (existing.last_refresh_at, new_catalog.last_refresh_at) {
                            if new_ts < existing_ts {
                                return Err(anyhow::Error::new(crate::devin_catalog::DevinDiscoveryError::StalePublication));
                            }
                        }
                    }
                }

                let new_members: Vec<Arc<AccountMember>> = current_snapshot
                    .members
                    .iter()
                    .map(|m| {
                        if m.id == target_id {
                            Arc::new(m.clone_for_snapshot(Some(Arc::clone(&new_catalog))))
                        } else {
                            Arc::clone(m)
                        }
                    })
                    .collect();

                let next_gen = self.generation_seq.fetch_add(1, Ordering::SeqCst) + 1;
                let candidate = compute_candidate_composition(
                    next_gen,
                    new_members,
                    Arc::clone(&current_snapshot.registry),
                    self.models_env.as_deref(),
                )
                .map_err(|_| {
                    crate::devin_catalog::DevinDiscoveryError::Internal(
                        "failed to compute composition",
                    )
                })?;

                // Deferred publication: cache is only populated AFTER candidate composition succeeds
                cache.insert(new_catalog.key.clone(), Arc::clone(&new_catalog));

                let arc_candidate = Arc::new(candidate);
                self.pool.store(Arc::clone(&arc_candidate));
                Ok(arc_candidate)
            })
            .map_err(|e| {
                if let Some(err) = e.downcast_ref::<crate::devin_catalog::DevinDiscoveryError>() {
                    match err {
                        crate::devin_catalog::DevinDiscoveryError::StalePublication => {
                            crate::devin_catalog::DevinDiscoveryError::StalePublication
                        }
                        _ => crate::devin_catalog::DevinDiscoveryError::Internal(
                            "publication coordinator failed",
                        ),
                    }
                } else {
                    crate::devin_catalog::DevinDiscoveryError::Internal(
                        "publication coordinator failed",
                    )
                }
            })
    }
}

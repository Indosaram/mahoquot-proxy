use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use mahoquot_registry::{
    CatalogSource, ModelDescriptor, ModelId, ProviderBinding, ProviderId, ProviderPolicy,
    RegistryError, RegistrySnapshot,
};

use crate::account::{AccountMember, ProviderKind};
use crate::models_route::ModelEntry;

/// Combined immutable composition holding account membership, effective model list,
/// and the catalog registry snapshot at a single monotonic generation.
#[derive(Clone)]
pub struct PoolSnapshot {
    pub generation: u64,
    pub members: Vec<Arc<AccountMember>>,
    pub models: Vec<ModelEntry>,
    pub registry: Arc<RegistrySnapshot>,
}

pub type RuntimeComposition = PoolSnapshot;

impl PoolSnapshot {
    pub fn new(
        generation: u64,
        members: Vec<Arc<AccountMember>>,
        models: Vec<ModelEntry>,
        registry: Arc<RegistrySnapshot>,
    ) -> Self {
        Self {
            generation,
            members,
            models,
            registry,
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

    pub fn find_member(&self, id: &str) -> Option<Arc<AccountMember>> {
        self.members.iter().find(|m| m.id == id).cloned()
    }

    pub fn routable_accounts_for_model(&self, model: &str) -> Vec<Arc<AccountMember>> {
        let model_id = ModelId::new(model).ok();
        self.members
            .iter()
            .filter(|m| {
                if m.supports_model(model) {
                    return true;
                }
                if let Some(ref mid) = model_id {
                    if let Some(desc) = self.registry.get_model(mid) {
                        for prov_id in desc.bindings.keys() {
                            let matches = match prov_id.as_str() {
                                "antigravity" => m.kind() == ProviderKind::Antigravity,
                                "claude" => m.kind() == ProviderKind::Claude,
                                "cursor" => m.kind() == ProviderKind::Cursor,
                                "kiro" => m.kind() == ProviderKind::Kiro,
                                "vertex" => m.kind() == ProviderKind::Vertex,
                                "zcode" => m.kind() == ProviderKind::Zcode,
                                "codex" => m.kind() == ProviderKind::Codex,
                                other => m.provider_name() == other,
                            };
                            if matches {
                                let unsupported = m
                                    .unsupported_models
                                    .read()
                                    .unwrap_or_else(|p| p.into_inner());
                                if !unsupported.iter().any(|u| u == model) {
                                    return true;
                                }
                            }
                        }
                    }
                }
                false
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
}

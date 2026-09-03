//! Pure domain types and invariants for the Mahoquot runtime model registry.
//!
//! Pure crate invariant: NO tokio, reqwest, axum, account secrets, arc-swap, or gateway dependency.

use std::collections::{BTreeMap, BTreeSet};

pub const MAX_ALIAS_DEPTH: usize = 10;

#[derive(Debug, thiserror::Error, PartialEq, Eq, Clone)]
pub enum RegistryError {
    #[error("provider id must not be empty")]
    EmptyProviderId,
    #[error("invalid provider id '{id}': {reason}")]
    InvalidProviderId { id: String, reason: String },
    #[error("model id must not be empty")]
    EmptyModelId,
    #[error("invalid model id '{id}': {reason}")]
    InvalidModelId { id: String, reason: String },
    #[error("unknown model: {0}")]
    UnknownModel(ModelId),
    #[error("unknown provider: {0}")]
    UnknownProvider(ProviderId),
    #[error("alias cycle detected for '{alias}': {cycle:?}")]
    AliasCycle { alias: ModelId, cycle: Vec<ModelId> },
    #[error("alias depth exceeded for '{alias}': depth {depth} > max {max_depth}")]
    AliasDepthExceeded {
        alias: ModelId,
        depth: usize,
        max_depth: usize,
    },
    #[error("alias '{alias}' points to unknown target '{target}'")]
    UnknownAliasTarget { alias: ModelId, target: ModelId },
    #[error("duplicate binding for model '{model_id}' and provider '{provider_id}'")]
    DuplicateBinding {
        model_id: ModelId,
        provider_id: ProviderId,
    },
    #[error("unauthorized contribution: provider '{provider_id}' with policy '{policy}' cannot accept dynamic discovery for model '{model_id}'")]
    UnauthorizedContribution {
        provider_id: ProviderId,
        policy: ProviderPolicy,
        model_id: ModelId,
    },
    #[error("model '{model_id}' is excluded")]
    ModelExcluded { model_id: ModelId },
    #[error("no routable models registered")]
    NoRoutableModels,
    #[error("provider blackout: exclusions eliminate all fallback-routable models for provider '{provider_id}' without explicit override")]
    ProviderBlackout { provider_id: ProviderId },
    #[error("serialization error: {0}")]
    SerializationError(String),
    #[error("deserialization error: {0}")]
    DeserializationError(String),
}

#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ProviderId(String);

impl ProviderId {
    pub fn canonical(s: impl AsRef<str>) -> Result<Self, RegistryError> {
        let raw = s.as_ref().trim();
        let lower = raw.to_ascii_lowercase();
        let normalized = match lower.as_str() {
            "openai" => "codex",
            "anthropic" => "claude",
            "google-vertex" => "vertex",
            _ => lower.as_str(),
        };
        Self::new(normalized)
    }

    pub fn new(s: impl AsRef<str>) -> Result<Self, RegistryError> {
        let raw = s.as_ref();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(RegistryError::EmptyProviderId);
        }
        if raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(RegistryError::InvalidProviderId {
                id: raw.to_string(),
                reason: "provider id must not contain whitespace or control characters".to_string(),
            });
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn codex() -> Self {
        Self("codex".to_string())
    }

    pub fn antigravity() -> Self {
        Self("antigravity".to_string())
    }

    pub fn claude() -> Self {
        Self("claude".to_string())
    }

    pub fn cursor() -> Self {
        Self("cursor".to_string())
    }

    pub fn kiro() -> Self {
        Self("kiro".to_string())
    }

    pub fn vertex() -> Self {
        Self("vertex".to_string())
    }

    pub fn zcode() -> Self {
        Self("zcode".to_string())
    }
}

impl std::ops::Deref for ProviderId {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ProviderId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
#[serde(transparent)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(s: impl AsRef<str>) -> Result<Self, RegistryError> {
        let raw = s.as_ref();
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(RegistryError::EmptyModelId);
        }
        if raw.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(RegistryError::InvalidModelId {
                id: raw.to_string(),
                reason: "model id must not contain whitespace or control characters".to_string(),
            });
        }
        Ok(Self(trimmed.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::ops::Deref for ModelId {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Debug,
    serde::Serialize,
    serde::Deserialize,
    Default,
)]
#[serde(transparent)]
pub struct CatalogVersion(pub u64);

impl CatalogVersion {
    pub fn new(v: u64) -> Self {
        Self(v)
    }

    pub fn as_u64(&self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for CatalogVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

impl From<u64> for CatalogVersion {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum CatalogSource {
    EmbeddedFallback,
    LkgCache,
    RemoteSigned,
    Discovered,
    LocalOverride,
}

impl CatalogSource {
    pub fn precedence(&self) -> u8 {
        match self {
            CatalogSource::EmbeddedFallback => 1,
            CatalogSource::LkgCache => 2,
            CatalogSource::RemoteSigned => 3,
            CatalogSource::Discovered => 4,
            CatalogSource::LocalOverride => 5,
        }
    }

    pub fn is_higher_precedence_than(&self, other: CatalogSource) -> bool {
        self.precedence() > other.precedence()
    }
}

impl std::fmt::Display for CatalogSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogSource::EmbeddedFallback => write!(f, "embedded_fallback"),
            CatalogSource::LkgCache => write!(f, "lkg_cache"),
            CatalogSource::RemoteSigned => write!(f, "remote_signed"),
            CatalogSource::Discovered => write!(f, "discovered"),
            CatalogSource::LocalOverride => write!(f, "local_override"),
        }
    }
}

#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ProviderPolicy {
    Closed,
    Discovered,
    Open,
}

impl std::fmt::Display for ProviderPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderPolicy::Closed => write!(f, "closed"),
            ProviderPolicy::Discovered => write!(f, "discovered"),
            ProviderPolicy::Open => write!(f, "open"),
        }
    }
}

#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct AuthorityMask {
    pub models: bool,
    pub capabilities: bool,
    pub aliases: bool,
    pub prefixes: bool,
    pub upstream_id: bool,
}

impl AuthorityMask {
    pub const NONE: Self = Self {
        models: false,
        capabilities: false,
        aliases: false,
        prefixes: false,
        upstream_id: false,
    };
    pub const ALL: Self = Self {
        models: true,
        capabilities: true,
        aliases: true,
        prefixes: true,
        upstream_id: true,
    };
    pub const MODELS_ONLY: Self = Self {
        models: true,
        capabilities: false,
        aliases: false,
        prefixes: false,
        upstream_id: false,
    };
    pub const CAPABILITIES_ONLY: Self = Self {
        models: false,
        capabilities: true,
        aliases: false,
        prefixes: false,
        upstream_id: false,
    };

    pub fn union(&self, other: &Self) -> Self {
        Self {
            models: self.models || other.models,
            capabilities: self.capabilities || other.capabilities,
            aliases: self.aliases || other.aliases,
            prefixes: self.prefixes || other.prefixes,
            upstream_id: self.upstream_id || other.upstream_id,
        }
    }

    pub fn with_models(mut self, val: bool) -> Self {
        self.models = val;
        self
    }

    pub fn with_capabilities(mut self, val: bool) -> Self {
        self.capabilities = val;
        self
    }

    pub fn with_aliases(mut self, val: bool) -> Self {
        self.aliases = val;
        self
    }

    pub fn with_prefixes(mut self, val: bool) -> Self {
        self.prefixes = val;
        self
    }

    pub fn with_upstream_id(mut self, val: bool) -> Self {
        self.upstream_id = val;
        self
    }
}

impl Default for AuthorityMask {
    fn default() -> Self {
        Self::ALL
    }
}

#[derive(
    Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    Chat,
    Image,
    Video,
    Realtime,
    CountTokens,
    Embedding,
    Audio,
    Tools,
}

impl ModelCapability {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelCapability::Chat => "chat",
            ModelCapability::Image => "image",
            ModelCapability::Video => "video",
            ModelCapability::Realtime => "realtime",
            ModelCapability::CountTokens => "count_tokens",
            ModelCapability::Embedding => "embedding",
            ModelCapability::Audio => "audio",
            ModelCapability::Tools => "tools",
        }
    }

    pub fn from_str_loose(s: &str) -> Option<Self> {
        let normalized = s.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "chat" | "text" | "completions" => Some(ModelCapability::Chat),
            "image" | "images" => Some(ModelCapability::Image),
            "video" | "videos" => Some(ModelCapability::Video),
            "realtime" => Some(ModelCapability::Realtime),
            "count_tokens" | "counttokens" | "tokenize" => Some(ModelCapability::CountTokens),
            "embedding" | "embeddings" => Some(ModelCapability::Embedding),
            "audio" => Some(ModelCapability::Audio),
            "tools" | "function_calling" => Some(ModelCapability::Tools),
            _ => None,
        }
    }
}

impl std::fmt::Display for ModelCapability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct ProviderBinding {
    pub provider_id: ProviderId,
    pub policy: ProviderPolicy,
    pub source: CatalogSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_model_id: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<ModelCapability>,
    #[serde(default)]
    pub authority: AuthorityMask,
    #[serde(default)]
    pub priority: i32,
}

impl ProviderBinding {
    pub fn new(provider_id: ProviderId, policy: ProviderPolicy, source: CatalogSource) -> Self {
        Self {
            provider_id,
            policy,
            source,
            upstream_model_id: None,
            capabilities: BTreeSet::new(),
            authority: AuthorityMask::ALL,
            priority: 0,
        }
    }

    pub fn with_capabilities(mut self, caps: impl IntoIterator<Item = ModelCapability>) -> Self {
        self.capabilities = caps.into_iter().collect();
        self
    }

    pub fn with_upstream_id(mut self, id: impl Into<String>) -> Self {
        self.upstream_model_id = Some(id.into());
        self
    }

    pub fn with_priority(mut self, prio: i32) -> Self {
        self.priority = prio;
        self
    }

    pub fn with_authority(mut self, authority: AuthorityMask) -> Self {
        self.authority = authority;
        self
    }

    pub fn effective_upstream_id<'a>(&'a self, fallback: &'a ModelId) -> &'a str {
        self.upstream_model_id
            .as_deref()
            .unwrap_or(fallback.as_str())
    }

    /// Merges an incoming duplicate binding into self, respecting authority masks and precedence.
    pub fn merge_with(&mut self, incoming: &ProviderBinding) {
        let incoming_higher = incoming.source.is_higher_precedence_than(self.source);
        let same_source = self.source == incoming.source;
        let incoming_priority_higher = incoming.priority > self.priority;
        let incoming_wins = incoming_higher || (same_source && incoming_priority_higher);

        if incoming_wins {
            if incoming.authority.capabilities {
                self.capabilities = incoming.capabilities.clone();
            }
            if incoming.authority.upstream_id && incoming.upstream_model_id.is_some() {
                self.upstream_model_id = incoming.upstream_model_id.clone();
            }
            self.source = incoming.source;
            self.priority = incoming.priority;
            self.policy = incoming.policy;
        } else if same_source && incoming.priority == self.priority {
            if incoming.authority.capabilities {
                self.capabilities
                    .extend(incoming.capabilities.iter().copied());
            }
            if incoming.authority.upstream_id && self.upstream_model_id.is_none() {
                self.upstream_model_id = incoming.upstream_model_id.clone();
            }
        } else {
            if !self.authority.capabilities && incoming.authority.capabilities {
                self.capabilities = incoming.capabilities.clone();
            }
            if !self.authority.upstream_id
                && incoming.authority.upstream_id
                && incoming.upstream_model_id.is_some()
            {
                self.upstream_model_id = incoming.upstream_model_id.clone();
            }
        }

        self.authority = self.authority.union(&incoming.authority);
    }
}

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct ModelDescriptor {
    pub id: ModelId,
    pub owned_by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<ModelCapability>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<ProviderId, ProviderBinding>,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub aliases: BTreeSet<ModelId>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl ModelDescriptor {
    pub fn new(id: ModelId, owned_by: impl Into<String>) -> Self {
        Self {
            id,
            owned_by: owned_by.into(),
            display_name: None,
            capabilities: BTreeSet::new(),
            bindings: BTreeMap::new(),
            aliases: BTreeSet::new(),
            metadata: BTreeMap::new(),
        }
    }

    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    pub fn with_capabilities(mut self, caps: impl IntoIterator<Item = ModelCapability>) -> Self {
        self.capabilities = caps.into_iter().collect();
        self
    }

    pub fn with_binding(mut self, binding: ProviderBinding) -> Self {
        self.bindings.insert(binding.provider_id.clone(), binding);
        self
    }

    pub fn with_alias(mut self, alias: ModelId) -> Self {
        self.aliases.insert(alias);
        self
    }

    pub fn has_capability(&self, cap: ModelCapability) -> bool {
        self.effective_capabilities().contains(&cap)
    }

    pub fn effective_capabilities(&self) -> BTreeSet<ModelCapability> {
        let mut caps = self.capabilities.clone();
        for binding in self.bindings.values() {
            caps.extend(binding.capabilities.iter().copied());
        }
        caps
    }

    pub fn binding_for(&self, provider: &ProviderId) -> Option<&ProviderBinding> {
        self.bindings.get(provider)
    }
}

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct DiscoveredModel {
    pub id: ModelId,
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub capabilities: BTreeSet<ModelCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u64>,
    #[serde(default)]
    pub authority: AuthorityMask,
}

impl DiscoveredModel {
    pub fn new(id: ModelId) -> Self {
        Self {
            id,
            capabilities: BTreeSet::new(),
            context_limit: None,
            authority: AuthorityMask::ALL,
        }
    }

    pub fn with_capabilities(mut self, caps: impl IntoIterator<Item = ModelCapability>) -> Self {
        self.capabilities = caps.into_iter().collect();
        self
    }

    pub fn with_context_limit(mut self, limit: u64) -> Self {
        self.context_limit = Some(limit);
        self
    }

    pub fn with_authority(mut self, authority: AuthorityMask) -> Self {
        self.authority = authority;
        self
    }
}

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub enum ContributionItem {
    Descriptor(ModelDescriptor),
    Discovered(DiscoveredModel),
}

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProviderContribution {
    pub provider_id: ProviderId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy: Option<ProviderPolicy>,
    pub models: Vec<ContributionItem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<ModelAliasRule>,
}

impl ProviderContribution {
    pub fn new(provider_id: ProviderId, models: Vec<ContributionItem>) -> Self {
        Self {
            provider_id,
            policy: None,
            models,
            aliases: Vec::new(),
        }
    }

    pub fn with_policy(mut self, policy: ProviderPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn with_aliases(mut self, aliases: impl IntoIterator<Item = ModelAliasRule>) -> Self {
        self.aliases = aliases.into_iter().collect();
        self
    }

    pub fn policy(&self) -> ProviderPolicy {
        self.policy.unwrap_or(ProviderPolicy::Closed)
    }

    pub fn from_descriptors(provider_id: ProviderId, descriptors: Vec<ModelDescriptor>) -> Self {
        Self {
            provider_id,
            policy: None,
            models: descriptors
                .into_iter()
                .map(ContributionItem::Descriptor)
                .collect(),
            aliases: Vec::new(),
        }
    }

    pub fn from_discovered_models(
        provider_id: ProviderId,
        discovered: Vec<DiscoveredModel>,
    ) -> Self {
        Self {
            provider_id,
            policy: None,
            models: discovered
                .into_iter()
                .map(ContributionItem::Discovered)
                .collect(),
            aliases: Vec::new(),
        }
    }

    pub fn descriptors(&self) -> Vec<&ModelDescriptor> {
        self.models
            .iter()
            .filter_map(|item| match item {
                ContributionItem::Descriptor(desc) => Some(desc),
                _ => None,
            })
            .collect()
    }

    pub fn model_ids(&self) -> Vec<ModelId> {
        self.models
            .iter()
            .map(|item| match item {
                ContributionItem::Descriptor(desc) => desc.id.clone(),
                ContributionItem::Discovered(disc) => disc.id.clone(),
            })
            .collect()
    }

    pub fn supported_model_ids(&self) -> Vec<String> {
        self.models
            .iter()
            .map(|item| match item {
                ContributionItem::Descriptor(desc) => desc.id.as_str().to_string(),
                ContributionItem::Discovered(disc) => disc.id.as_str().to_string(),
            })
            .collect()
    }

    pub fn has_prefix_support(&self) -> bool {
        self.models.iter().any(|item| match item {
            ContributionItem::Descriptor(desc) => desc
                .binding_for(&self.provider_id)
                .map(|b| b.authority.prefixes)
                .unwrap_or(false),
            ContributionItem::Discovered(disc) => disc.authority.prefixes,
        })
    }

    pub fn supports_model(&self, model: &str) -> bool {
        let unstripped_kiro = model.strip_prefix("kiro/").unwrap_or(model);
        for item in &self.models {
            match item {
                ContributionItem::Descriptor(desc) => {
                    if desc.id.as_str() == model {
                        return true;
                    }
                    if let Some(binding) = desc.binding_for(&self.provider_id) {
                        let upstream = binding.effective_upstream_id(&desc.id);
                        if upstream == model || upstream == unstripped_kiro {
                            return true;
                        }
                    }
                    if desc.id.as_str() == format!("kiro/{unstripped_kiro}") {
                        return true;
                    }
                    if desc.aliases.iter().any(|a| a.as_str() == model) {
                        return true;
                    }
                }
                ContributionItem::Discovered(disc) => {
                    if disc.id.as_str() == model {
                        return true;
                    }
                }
            }
        }
        for a in &self.aliases {
            if a.alias.as_str() == model {
                return true;
            }
        }
        if self.provider_id == ProviderId::vertex()
            && self.has_prefix_support()
            && (model.starts_with("gemini-") || model.starts_with("google/"))
        {
            return true;
        }
        false
    }

    pub fn capability_profile(&self, model: &str) -> Option<BTreeSet<ModelCapability>> {
        let unstripped_kiro = model.strip_prefix("kiro/").unwrap_or(model);
        for item in &self.models {
            match item {
                ContributionItem::Descriptor(desc) => {
                    if desc.id.as_str() == model {
                        return Some(desc.effective_capabilities());
                    }
                    if let Some(binding) = desc.binding_for(&self.provider_id) {
                        let upstream = binding.effective_upstream_id(&desc.id);
                        if upstream == model || upstream == unstripped_kiro {
                            return Some(desc.effective_capabilities());
                        }
                    }
                    if desc.aliases.iter().any(|a| a.as_str() == model) {
                        return Some(desc.effective_capabilities());
                    }
                }
                ContributionItem::Discovered(disc) => {
                    if disc.id.as_str() == model {
                        return Some(disc.capabilities.clone());
                    }
                }
            }
        }
        for a in &self.aliases {
            if a.alias.as_str() == model {
                return self.capability_profile(a.target.as_str());
            }
        }
        if self.provider_id == ProviderId::vertex()
            && self.has_prefix_support()
            && (model.starts_with("gemini-") || model.starts_with("google/"))
        {
            let mut caps = BTreeSet::new();
            caps.insert(ModelCapability::Chat);
            caps.insert(ModelCapability::Tools);
            return Some(caps);
        }
        None
    }
}

#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct ModelAliasRule {
    pub alias: ModelId,
    pub target: ModelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<ProviderId>,
}

#[derive(
    Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, serde::Serialize, serde::Deserialize,
)]
pub struct ModelExclusionRule {
    pub model_id: ModelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<ProviderId>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ResolvedModel {
    pub canonical_id: ModelId,
    pub descriptor: Option<ModelDescriptor>,
    pub eligible_bindings: Vec<ProviderBinding>,
    pub effective_capabilities: BTreeSet<ModelCapability>,
    pub source: CatalogSource,
}

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
pub struct RegistrySnapshot {
    pub version: CatalogVersion,
    pub source: CatalogSource,
    pub models: BTreeMap<ModelId, ModelDescriptor>,
    pub providers: BTreeMap<ProviderId, ProviderPolicy>,
    pub aliases: BTreeMap<ModelId, ModelAliasRule>,
    pub exclusions: BTreeSet<ModelExclusionRule>,
}

impl RegistrySnapshot {
    pub fn version(&self) -> CatalogVersion {
        self.version
    }

    pub fn source(&self) -> CatalogSource {
        self.source
    }

    pub fn models(&self) -> &BTreeMap<ModelId, ModelDescriptor> {
        &self.models
    }

    pub fn providers(&self) -> &BTreeMap<ProviderId, ProviderPolicy> {
        &self.providers
    }

    pub fn aliases(&self) -> &BTreeMap<ModelId, ModelAliasRule> {
        &self.aliases
    }

    pub fn exclusions(&self) -> &BTreeSet<ModelExclusionRule> {
        &self.exclusions
    }

    pub fn get_model(&self, id: &ModelId) -> Option<&ModelDescriptor> {
        self.models.get(id)
    }

    pub fn resolve_alias(&self, requested: &ModelId) -> Result<ModelId, RegistryError> {
        let mut current = requested.clone();
        let mut visited = Vec::new();

        while let Some(rule) = self.aliases.get(&current) {
            if visited.contains(&current) {
                visited.push(current.clone());
                return Err(RegistryError::AliasCycle {
                    alias: requested.clone(),
                    cycle: visited,
                });
            }
            if visited.len() >= MAX_ALIAS_DEPTH {
                return Err(RegistryError::AliasDepthExceeded {
                    alias: requested.clone(),
                    depth: visited.len() + 1,
                    max_depth: MAX_ALIAS_DEPTH,
                });
            }
            visited.push(current.clone());
            current = rule.target.clone();
        }

        Ok(current)
    }

    pub fn resolve(&self, requested: &str) -> Result<ResolvedModel, RegistryError> {
        let req_id = ModelId::new(requested)?;
        let canonical_id = self.resolve_alias(&req_id)?;

        // Global exclusion check
        if self.exclusions.contains(&ModelExclusionRule {
            model_id: canonical_id.clone(),
            provider_id: None,
        }) {
            return Err(RegistryError::ModelExcluded {
                model_id: canonical_id,
            });
        }

        let open_providers: Vec<ProviderId> = self
            .providers
            .iter()
            .filter(|(_, &policy)| policy == ProviderPolicy::Open)
            .map(|(id, _)| id.clone())
            .collect();

        if let Some(descriptor) = self.models.get(&canonical_id) {
            // Find eligible bindings, excluding provider-specific exclusions
            let mut closed_or_discovered: Vec<ProviderBinding> = Vec::new();
            let mut open_bindings: Vec<ProviderBinding> = Vec::new();

            for binding in descriptor.bindings.values() {
                if self.exclusions.contains(&ModelExclusionRule {
                    model_id: canonical_id.clone(),
                    provider_id: Some(binding.provider_id.clone()),
                }) {
                    continue;
                }

                match binding.policy {
                    ProviderPolicy::Closed | ProviderPolicy::Discovered => {
                        closed_or_discovered.push(binding.clone());
                    }
                    ProviderPolicy::Open => {
                        open_bindings.push(binding.clone());
                    }
                }
            }

            // If closed or discovered bindings exist, they claim this model;
            // Open Codex CANNOT steal a model claimed by closed or discovered bindings.
            let eligible_bindings = if !closed_or_discovered.is_empty() {
                closed_or_discovered.sort_by(|a, b| {
                    b.priority
                        .cmp(&a.priority)
                        .then_with(|| b.source.precedence().cmp(&a.source.precedence()))
                        .then_with(|| a.provider_id.cmp(&b.provider_id))
                });
                closed_or_discovered
            } else if !open_bindings.is_empty() {
                open_bindings.sort_by(|a, b| {
                    b.priority
                        .cmp(&a.priority)
                        .then_with(|| b.source.precedence().cmp(&a.source.precedence()))
                        .then_with(|| a.provider_id.cmp(&b.provider_id))
                });
                open_bindings
            } else if !open_providers.is_empty() {
                // No bindings on descriptor, but open provider registered
                let mut synthesized = Vec::new();
                for pid in &open_providers {
                    if !self.exclusions.contains(&ModelExclusionRule {
                        model_id: canonical_id.clone(),
                        provider_id: Some(pid.clone()),
                    }) {
                        synthesized.push(
                            ProviderBinding::new(
                                pid.clone(),
                                ProviderPolicy::Open,
                                CatalogSource::EmbeddedFallback,
                            )
                            .with_capabilities([ModelCapability::Chat]),
                        );
                    }
                }
                synthesized
            } else {
                Vec::new()
            };

            if eligible_bindings.is_empty() {
                return Err(RegistryError::UnknownModel(canonical_id));
            }

            let effective_caps = descriptor.effective_capabilities();
            let primary_source = eligible_bindings
                .first()
                .map(|b| b.source)
                .unwrap_or(self.source);

            Ok(ResolvedModel {
                canonical_id,
                descriptor: Some(descriptor.clone()),
                eligible_bindings,
                effective_capabilities: effective_caps,
                source: primary_source,
            })
        } else {
            // Model not explicitly in catalog: only Open providers can serve unclaimed models
            if open_providers.is_empty() {
                return Err(RegistryError::UnknownModel(canonical_id));
            }

            let mut eligible_bindings = Vec::new();
            for pid in &open_providers {
                if !self.exclusions.contains(&ModelExclusionRule {
                    model_id: canonical_id.clone(),
                    provider_id: Some(pid.clone()),
                }) {
                    eligible_bindings.push(
                        ProviderBinding::new(
                            pid.clone(),
                            ProviderPolicy::Open,
                            CatalogSource::EmbeddedFallback,
                        )
                        .with_capabilities([ModelCapability::Chat]),
                    );
                }
            }

            if eligible_bindings.is_empty() {
                return Err(RegistryError::UnknownModel(canonical_id));
            }

            let mut caps = BTreeSet::new();
            caps.insert(ModelCapability::Chat);

            Ok(ResolvedModel {
                canonical_id,
                descriptor: None,
                eligible_bindings,
                effective_capabilities: caps,
                source: CatalogSource::EmbeddedFallback,
            })
        }
    }

    pub fn capabilities_for(
        &self,
        model: &str,
    ) -> Result<BTreeSet<ModelCapability>, RegistryError> {
        let resolved = self.resolve(model)?;
        Ok(resolved.effective_capabilities)
    }

    pub fn contribution_for_provider(&self, provider_id: &ProviderId) -> ProviderContribution {
        let policy = self
            .providers
            .get(provider_id)
            .copied()
            .unwrap_or(ProviderPolicy::Closed);

        let mut descriptors = Vec::new();
        for desc in self.models.values() {
            if desc.bindings.contains_key(provider_id) {
                descriptors.push(desc.clone());
            }
        }

        let mut aliases = Vec::new();
        for rule in self.aliases.values() {
            if rule.provider_id.as_ref() == Some(provider_id) {
                aliases.push(rule.clone());
            } else if rule.provider_id.is_none() {
                if let Some(target_desc) = self.models.get(&rule.target) {
                    if target_desc.bindings.contains_key(provider_id) {
                        aliases.push(rule.clone());
                    }
                }
            }
        }

        ProviderContribution::from_descriptors(provider_id.clone(), descriptors)
            .with_policy(policy)
            .with_aliases(aliases)
    }

    pub fn to_json_canonical(&self) -> Result<String, RegistryError> {
        serde_json::to_string_pretty(self)
            .map_err(|e| RegistryError::SerializationError(e.to_string()))
    }

    pub fn from_json(raw: &str) -> Result<Self, RegistryError> {
        serde_json::from_str(raw).map_err(|e| RegistryError::DeserializationError(e.to_string()))
    }

    pub fn validate(&self) -> Result<(), RegistryError> {
        let has_open_provider = self.providers.values().any(|&p| p == ProviderPolicy::Open);

        // Validate provider IDs
        for pid in self.providers.keys() {
            ProviderId::new(pid.as_str())?;
        }

        // Validate models and bindings
        for (model_id, desc) in &self.models {
            ModelId::new(model_id.as_str())?;
            if model_id != &desc.id {
                return Err(RegistryError::InvalidModelId {
                    id: model_id.to_string(),
                    reason: format!(
                        "models map key '{}' does not match descriptor id '{}'",
                        model_id, desc.id
                    ),
                });
            }
            for (binding_pid, binding) in &desc.bindings {
                ProviderId::new(binding_pid.as_str())?;
                if binding_pid != &binding.provider_id {
                    return Err(RegistryError::InvalidProviderId {
                        id: binding_pid.to_string(),
                        reason: format!(
                            "binding map key '{}' does not match binding provider_id '{}'",
                            binding_pid, binding.provider_id
                        ),
                    });
                }
                if !self.providers.contains_key(binding_pid) {
                    return Err(RegistryError::UnknownProvider(binding_pid.clone()));
                }
            }
        }

        // Validate aliases
        for (alias, rule) in &self.aliases {
            ModelId::new(alias.as_str())?;
            ModelId::new(rule.target.as_str())?;
            if let Some(pid) = &rule.provider_id {
                ProviderId::new(pid.as_str())?;
            }

            let mut current = rule.target.clone();
            let mut visited = vec![alias.clone()];

            if *alias == current {
                visited.push(current.clone());
                return Err(RegistryError::AliasCycle {
                    alias: alias.clone(),
                    cycle: visited,
                });
            }

            while let Some(next_rule) = self.aliases.get(&current) {
                if visited.contains(&current) {
                    visited.push(current.clone());
                    return Err(RegistryError::AliasCycle {
                        alias: alias.clone(),
                        cycle: visited,
                    });
                }
                if visited.len() >= MAX_ALIAS_DEPTH {
                    return Err(RegistryError::AliasDepthExceeded {
                        alias: alias.clone(),
                        depth: visited.len() + 1,
                        max_depth: MAX_ALIAS_DEPTH,
                    });
                }
                visited.push(current.clone());
                current = next_rule.target.clone();
            }

            if !self.models.contains_key(&current) && !has_open_provider {
                return Err(RegistryError::UnknownAliasTarget {
                    alias: alias.clone(),
                    target: current,
                });
            }
        }

        if self.models.is_empty() && !has_open_provider {
            return Err(RegistryError::NoRoutableModels);
        }

        Ok(())
    }

    /// Validates local configuration (aliases, exclusions, custom models, and explicit blackout overrides)
    /// against this active snapshot.
    ///
    /// Invariants:
    /// 1. Alias target must exist in active registry snapshot (cannot point to void).
    /// 2. Alias cycle detection: no direct or indirect cycles (`A -> B -> A`), max alias hop depth <= 10.
    /// 3. Exclusions must not eliminate 100% of fallback-routable models for a provider without explicit override.
    pub fn validate_local_composition(
        &self,
        aliases: &[ModelAliasRule],
        exclusions: &[ModelExclusionRule],
        custom_models: &[ModelDescriptor],
        allowed_blackouts: &[ProviderId],
    ) -> Result<(), RegistryError> {
        let mut all_models: BTreeSet<ModelId> = self.models.keys().cloned().collect();
        let mut all_providers: BTreeSet<ProviderId> = self.providers.keys().cloned().collect();

        // 1. Validate custom models and their bindings
        for desc in custom_models {
            ModelId::new(desc.id.as_str())?;
            for (pid, binding) in &desc.bindings {
                ProviderId::new(pid.as_str())?;
                if pid != &binding.provider_id {
                    return Err(RegistryError::InvalidProviderId {
                        id: pid.to_string(),
                        reason: format!(
                            "binding map key '{}' does not match binding provider_id '{}'",
                            pid, binding.provider_id
                        ),
                    });
                }
                all_providers.insert(pid.clone());
            }
            all_models.insert(desc.id.clone());
        }

        // 2. Validate aliases
        let mut combined_aliases: BTreeMap<ModelId, ModelAliasRule> = self.aliases.clone();
        for rule in aliases {
            ModelId::new(rule.alias.as_str())?;
            ModelId::new(rule.target.as_str())?;
            if let Some(pid) = &rule.provider_id {
                ProviderId::new(pid.as_str())?;
            }
            combined_aliases.insert(rule.alias.clone(), rule.clone());
        }

        for (alias, rule) in &combined_aliases {
            let mut current = rule.target.clone();
            let mut visited = vec![alias.clone()];

            if *alias == current {
                visited.push(current.clone());
                return Err(RegistryError::AliasCycle {
                    alias: alias.clone(),
                    cycle: visited,
                });
            }

            while let Some(next_rule) = combined_aliases.get(&current) {
                if visited.contains(&current) {
                    visited.push(current.clone());
                    return Err(RegistryError::AliasCycle {
                        alias: alias.clone(),
                        cycle: visited,
                    });
                }
                if visited.len() >= MAX_ALIAS_DEPTH {
                    return Err(RegistryError::AliasDepthExceeded {
                        alias: alias.clone(),
                        depth: visited.len() + 1,
                        max_depth: MAX_ALIAS_DEPTH,
                    });
                }
                visited.push(current.clone());
                current = next_rule.target.clone();
            }

            // Invariant 1: Alias target must exist in active registry snapshot (cannot point to void).
            if !all_models.contains(&current) {
                return Err(RegistryError::UnknownAliasTarget {
                    alias: alias.clone(),
                    target: current,
                });
            }
        }

        // 3. Validate exclusions
        for rule in exclusions {
            ModelId::new(rule.model_id.as_str())?;
            if let Some(pid) = &rule.provider_id {
                ProviderId::new(pid.as_str())?;
            }
        }

        // 4. Invariant 3: Exclusions must not eliminate 100% of fallback-routable models for a provider without explicit override.
        let allowed_blackout_set: BTreeSet<&ProviderId> = allowed_blackouts.iter().collect();

        for pid in &all_providers {
            if allowed_blackout_set.contains(pid) {
                continue;
            }

            let mut provider_models = BTreeSet::new();
            for desc in self.models.values() {
                if desc.bindings.contains_key(pid) {
                    provider_models.insert(desc.id.clone());
                }
            }
            for desc in custom_models {
                if desc.bindings.contains_key(pid) {
                    provider_models.insert(desc.id.clone());
                }
            }

            if provider_models.is_empty() {
                continue;
            }

            let mut routable_count = 0;
            for m in &provider_models {
                let is_globally_excluded = exclusions
                    .iter()
                    .any(|e| e.provider_id.is_none() && &e.model_id == m);
                let is_provider_excluded = exclusions
                    .iter()
                    .any(|e| e.provider_id.as_ref() == Some(pid) && &e.model_id == m);
                if !is_globally_excluded && !is_provider_excluded {
                    routable_count += 1;
                }
            }

            if routable_count == 0 {
                return Err(RegistryError::ProviderBlackout {
                    provider_id: pid.clone(),
                });
            }
        }

        Ok(())
    }

    /// Composes a candidate `RegistrySnapshot` incorporating validated local settings.
    pub fn compose_with_settings(
        &self,
        aliases: Vec<ModelAliasRule>,
        exclusions: BTreeSet<ModelExclusionRule>,
        custom_models: Vec<ModelDescriptor>,
        allowed_blackouts: &[ProviderId],
    ) -> Result<RegistrySnapshot, RegistryError> {
        let exclusions_vec: Vec<ModelExclusionRule> = exclusions.iter().cloned().collect();
        self.validate_local_composition(
            &aliases,
            &exclusions_vec,
            &custom_models,
            allowed_blackouts,
        )?;

        let mut models = self.models.clone();
        for desc in custom_models {
            models.insert(desc.id.clone(), desc);
        }

        let mut merged_aliases = self.aliases.clone();
        for rule in aliases {
            merged_aliases.insert(rule.alias.clone(), rule);
        }

        let mut merged_exclusions = self.exclusions.clone();
        merged_exclusions.extend(exclusions);

        let snapshot = RegistrySnapshot {
            version: self.version,
            source: CatalogSource::LocalOverride,
            models,
            providers: self.providers.clone(),
            aliases: merged_aliases,
            exclusions: merged_exclusions,
        };

        Ok(snapshot)
    }
}

#[derive(Debug)]
pub struct RegistryBuilder {
    version: CatalogVersion,
    source: CatalogSource,
    providers: BTreeMap<ProviderId, ProviderPolicy>,
    models: BTreeMap<ModelId, ModelDescriptor>,
    aliases: BTreeMap<ModelId, ModelAliasRule>,
    exclusions: BTreeSet<ModelExclusionRule>,
}

impl RegistryBuilder {
    pub fn new(version: CatalogVersion, source: CatalogSource) -> Self {
        Self {
            version,
            source,
            providers: BTreeMap::new(),
            models: BTreeMap::new(),
            aliases: BTreeMap::new(),
            exclusions: BTreeSet::new(),
        }
    }

    pub fn register_provider(&mut self, id: ProviderId, policy: ProviderPolicy) -> &mut Self {
        self.providers.insert(id, policy);
        self
    }

    pub fn add_model(&mut self, descriptor: ModelDescriptor) -> Result<&mut Self, RegistryError> {
        match self.models.get_mut(&descriptor.id) {
            Some(existing) => {
                if existing.owned_by.is_empty() && !descriptor.owned_by.is_empty() {
                    existing.owned_by = descriptor.owned_by;
                }
                if existing.display_name.is_none() && descriptor.display_name.is_some() {
                    existing.display_name = descriptor.display_name;
                }
                existing.capabilities.extend(descriptor.capabilities);
                existing.aliases.extend(descriptor.aliases);
                existing.metadata.extend(descriptor.metadata);
                for (pid, incoming_binding) in descriptor.bindings {
                    match existing.bindings.get_mut(&pid) {
                        Some(b) => b.merge_with(&incoming_binding),
                        None => {
                            existing.bindings.insert(pid, incoming_binding);
                        }
                    }
                }
            }
            None => {
                self.models.insert(descriptor.id.clone(), descriptor);
            }
        }
        Ok(self)
    }

    pub fn add_binding(
        &mut self,
        model_id: ModelId,
        binding: ProviderBinding,
    ) -> Result<&mut Self, RegistryError> {
        let model = self
            .models
            .entry(model_id.clone())
            .or_insert_with(|| ModelDescriptor::new(model_id.clone(), ""));
        if model.bindings.contains_key(&binding.provider_id) {
            return Err(RegistryError::DuplicateBinding {
                model_id,
                provider_id: binding.provider_id,
            });
        }
        model.bindings.insert(binding.provider_id.clone(), binding);
        Ok(self)
    }

    pub fn merge_binding(
        &mut self,
        model_id: ModelId,
        binding: ProviderBinding,
    ) -> Result<&mut Self, RegistryError> {
        let model = self
            .models
            .entry(model_id.clone())
            .or_insert_with(|| ModelDescriptor::new(model_id.clone(), ""));
        match model.bindings.get_mut(&binding.provider_id) {
            Some(existing) => existing.merge_with(&binding),
            None => {
                model.bindings.insert(binding.provider_id.clone(), binding);
            }
        }
        Ok(self)
    }

    pub fn apply_contribution(
        &mut self,
        contribution: ProviderContribution,
    ) -> Result<&mut Self, RegistryError> {
        if let Some(policy) = contribution.policy {
            if !self.providers.contains_key(&contribution.provider_id) {
                self.register_provider(contribution.provider_id.clone(), policy);
            }
        }
        let policy = match self.providers.get(&contribution.provider_id) {
            Some(&p) => p,
            None => return Err(RegistryError::UnknownProvider(contribution.provider_id)),
        };

        for item in contribution.models {
            match item {
                ContributionItem::Descriptor(desc) => {
                    self.add_model(desc)?;
                }
                ContributionItem::Discovered(disc) => {
                    match policy {
                        ProviderPolicy::Closed => {
                            // Closed providers CANNOT accept dynamic discovery contributions
                            return Err(RegistryError::UnauthorizedContribution {
                                provider_id: contribution.provider_id,
                                policy,
                                model_id: disc.id,
                            });
                        }
                        ProviderPolicy::Discovered | ProviderPolicy::Open => {
                            // For discovered or open providers, add or merge the model.
                            // Invariant: Discovered models cannot overwrite catalog capabilities.
                            // If the model already has catalog-declared capabilities (e.g. from RemoteSigned/Embedded/LKG),
                            // discovery cannot wipe or override them.
                            let mut binding = ProviderBinding::new(
                                contribution.provider_id.clone(),
                                policy,
                                CatalogSource::Discovered,
                            )
                            .with_authority(disc.authority);

                            if disc.authority.capabilities {
                                binding = binding.with_capabilities(disc.capabilities.clone());
                            }

                            let model = self.models.entry(disc.id.clone()).or_insert_with(|| {
                                ModelDescriptor::new(
                                    disc.id.clone(),
                                    contribution.provider_id.as_str(),
                                )
                            });

                            if let Some(limit) = disc.context_limit {
                                model
                                    .metadata
                                    .insert("context_limit".to_string(), limit.to_string());
                            }

                            match model.bindings.get_mut(&contribution.provider_id) {
                                Some(existing) => {
                                    // Discovered model cannot overwrite catalog capabilities or lower their scope.
                                    // Catalog sources (Embedded, LKG, RemoteSigned, LocalOverride) are authoritative over discovery.
                                    let is_existing_catalog =
                                        existing.source != CatalogSource::Discovered;
                                    if is_existing_catalog {
                                        // Discovery can only supplement missing fields if its mask permits, but cannot wipe or replace catalog capabilities
                                        if disc.authority.capabilities
                                            && !disc.capabilities.is_empty()
                                        {
                                            existing
                                                .capabilities
                                                .extend(disc.capabilities.iter().copied());
                                        }
                                        if disc.authority.upstream_id
                                            && existing.upstream_model_id.is_none()
                                        {
                                            existing.upstream_model_id =
                                                binding.upstream_model_id.clone();
                                        }
                                        existing.authority =
                                            existing.authority.union(&disc.authority);
                                    } else {
                                        existing.merge_with(&binding);
                                    }
                                }
                                None => {
                                    model
                                        .bindings
                                        .insert(contribution.provider_id.clone(), binding);
                                }
                            }
                        }
                    }
                }
            }
        }

        for alias in contribution.aliases {
            self.add_alias_rule(alias)?;
        }

        Ok(self)
    }

    pub fn add_alias_rule(&mut self, rule: ModelAliasRule) -> Result<&mut Self, RegistryError> {
        self.add_alias(rule.alias, rule.target, rule.provider_id)
    }

    pub fn add_alias(
        &mut self,
        alias: ModelId,
        target: ModelId,
        provider_id: Option<ProviderId>,
    ) -> Result<&mut Self, RegistryError> {
        self.aliases.insert(
            alias.clone(),
            ModelAliasRule {
                alias,
                target,
                provider_id,
            },
        );
        Ok(self)
    }

    pub fn add_exclusion(
        &mut self,
        model_id: ModelId,
        provider_id: Option<ProviderId>,
    ) -> &mut Self {
        self.exclusions.insert(ModelExclusionRule {
            model_id,
            provider_id,
        });
        self
    }

    pub fn build(self) -> Result<RegistrySnapshot, RegistryError> {
        let has_open_provider = self.providers.values().any(|&p| p == ProviderPolicy::Open);

        // Validate aliases
        for (alias, rule) in &self.aliases {
            let mut current = rule.target.clone();
            let mut visited = vec![alias.clone()];

            if *alias == current {
                visited.push(current.clone());
                return Err(RegistryError::AliasCycle {
                    alias: alias.clone(),
                    cycle: visited,
                });
            }

            while let Some(next_rule) = self.aliases.get(&current) {
                if visited.contains(&current) {
                    visited.push(current.clone());
                    return Err(RegistryError::AliasCycle {
                        alias: alias.clone(),
                        cycle: visited,
                    });
                }
                if visited.len() >= MAX_ALIAS_DEPTH {
                    return Err(RegistryError::AliasDepthExceeded {
                        alias: alias.clone(),
                        depth: visited.len() + 1,
                        max_depth: MAX_ALIAS_DEPTH,
                    });
                }
                visited.push(current.clone());
                current = next_rule.target.clone();
            }

            if !self.models.contains_key(&current) && !has_open_provider {
                return Err(RegistryError::UnknownAliasTarget {
                    alias: alias.clone(),
                    target: current,
                });
            }
        }

        if self.models.is_empty() && !has_open_provider {
            return Err(RegistryError::NoRoutableModels);
        }

        Ok(RegistrySnapshot {
            version: self.version,
            source: self.source,
            models: self.models,
            providers: self.providers,
            aliases: self.aliases,
            exclusions: self.exclusions,
        })
    }
}

pub mod envelope;
pub use envelope::*;

pub const EMBEDDED_CATALOG_JSON: &str = include_str!("../catalog/models-v1.json");

pub fn embedded_catalog() -> &'static str {
    EMBEDDED_CATALOG_JSON
}

pub fn embedded_catalog_bytes() -> &'static [u8] {
    EMBEDDED_CATALOG_JSON.as_bytes()
}

pub fn embedded_registry_snapshot() -> Result<RegistrySnapshot, RegistryError> {
    let snapshot = RegistrySnapshot::from_json(embedded_catalog())?;
    snapshot.validate()?;
    Ok(snapshot)
}

static EMBEDDED_SNAPSHOT: std::sync::OnceLock<RegistrySnapshot> = std::sync::OnceLock::new();

pub fn embedded_snapshot() -> &'static RegistrySnapshot {
    EMBEDDED_SNAPSHOT.get_or_init(|| {
        embedded_registry_snapshot()
            .expect("embedded catalog must be valid JSON matching RegistrySnapshot schema")
    })
}

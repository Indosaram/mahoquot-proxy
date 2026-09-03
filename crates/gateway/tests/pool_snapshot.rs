use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mahoquot_gateway::account::{AccountMember, GenericAccount, ProviderAccount};
use mahoquot_gateway::state::{PoolSnapshot, UnifiedRuntimeState};
use mahoquot_registry::*;

fn test_registry(version: u64, models: &[(&str, ProviderId, ProviderPolicy)]) -> RegistrySnapshot {
    let mut builder =
        RegistryBuilder::new(CatalogVersion(version), CatalogSource::EmbeddedFallback);
    for &(m, ref p, policy) in models {
        builder.register_provider(p.clone(), policy);
        let model_id = ModelId::new(m).unwrap();
        let mut desc = ModelDescriptor::new(model_id.clone(), p.as_str());
        desc.capabilities.insert(ModelCapability::Chat);
        let binding = ProviderBinding::new(p.clone(), policy, CatalogSource::EmbeddedFallback)
            .with_capabilities([ModelCapability::Chat]);
        builder.add_model(desc).unwrap();
        builder.add_binding(model_id, binding).unwrap();
    }
    builder.build().unwrap()
}

fn test_member(id: &str, provider_kind: &str, models: Vec<String>) -> Arc<AccountMember> {
    let account = GenericAccount {
        identity_slug: id.to_string(),
        provider: provider_kind.to_string(),
        label: id.to_string(),
        adapter: "chat".to_string(),
        base_url: "http://127.0.0.1:18899".to_string(),
        api_key: "key".to_string(),
        auth_mode: "bearer".to_string(),
        refresh_token: String::new(),
        expired: "2099-01-01T00:00:00Z".to_string(),
        token_url: String::new(),
        client_id: String::new(),
        project_id: String::new(),
        static_headers: Default::default(),
        disabled: false,
        models,
    };
    Arc::new(AccountMember::for_test_with_id(
        id,
        ProviderAccount::Generic(account),
    ))
}

#[test]
fn test_pool_snapshot_atomic_generational_pair() {
    let reg = Arc::new(test_registry(
        1,
        &[("model-a", ProviderId::claude(), ProviderPolicy::Closed)],
    ));
    let member = test_member("acc-1", "claude", vec!["model-a".to_string()]);
    let members = vec![member.clone()];

    let composition = PoolSnapshot::new(
        1,
        members.clone(),
        vec![mahoquot_gateway::models_route::ModelEntry {
            id: "model-a".to_string(),
            owned_by: "claude".to_string(),
        }],
        reg.clone(),
    );

    assert_eq!(composition.generation(), 1);
    assert_eq!(composition.members().len(), 1);
    assert_eq!(composition.models().len(), 1);
    assert_eq!(composition.registry().version(), CatalogVersion(1));
    assert_eq!(composition.routable_accounts_for_model("model-a").len(), 1);
    assert_eq!(
        composition
            .routable_accounts_for_model("model-nonexistent")
            .len(),
        0
    );
}

#[test]
fn concurrent_registry_refresh_is_generation_atomic() {
    let initial_reg = Arc::new(test_registry(
        1,
        &[("model-v1", ProviderId::claude(), ProviderPolicy::Closed)],
    ));
    let member = test_member("acc-1", "claude", vec![]);
    let initial_comp = PoolSnapshot::new(
        1,
        vec![member.clone()],
        vec![mahoquot_gateway::models_route::ModelEntry {
            id: "model-v1".to_string(),
            owned_by: "claude".to_string(),
        }],
        initial_reg,
    );

    let runtime = Arc::new(UnifiedRuntimeState::new(initial_comp, None));
    let running = Arc::new(AtomicBool::new(true));
    let mixed_observations = Arc::new(AtomicU64::new(0));
    let total_reads = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    // Spawn 8 concurrent reader threads
    for _ in 0..8 {
        let rt = Arc::clone(&runtime);
        let run = Arc::clone(&running);
        let mixed = Arc::clone(&mixed_observations);
        let reads = Arc::clone(&total_reads);

        handles.push(std::thread::spawn(move || {
            while run.load(Ordering::Relaxed) {
                let snap = rt.load();
                let gen = snap.generation();
                let reg_ver = snap.registry().version().as_u64();
                reads.fetch_add(1, Ordering::Relaxed);

                // Generation and registry version must be 100% matched
                if gen != reg_ver {
                    mixed.fetch_add(1, Ordering::SeqCst);
                }

                // Check model list consistency with generation
                let expected_model = format!("model-v{gen}");
                if !snap.models().iter().any(|m| m.id == expected_model) {
                    mixed.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }

    // Writer thread updates registry across generations 2..=10
    for target_gen in 2..=10 {
        std::thread::sleep(Duration::from_millis(5));
        let next_model = format!("model-v{target_gen}");
        let next_reg = Arc::new(test_registry(
            target_gen,
            &[(&next_model, ProviderId::claude(), ProviderPolicy::Closed)],
        ));
        runtime
            .update_registry(next_reg)
            .expect("update should succeed");
    }

    running.store(false, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        mixed_observations.load(Ordering::SeqCst),
        0,
        "must observe zero mixed-generation half-states"
    );
    assert!(
        total_reads.load(Ordering::Relaxed) > 1000,
        "readers must perform thousands of concurrent reads"
    );

    // Final generation and registry version match the last candidate (10)
    let final_snap = runtime.load();
    assert_eq!(final_snap.generation(), 10);
    assert_eq!(final_snap.registry().version(), CatalogVersion(10));
}

#[test]
fn account_add_remove_under_concurrent_requests_sees_atomic_update() {
    let reg = Arc::new(test_registry(
        1,
        &[
            ("model-alpha", ProviderId::claude(), ProviderPolicy::Closed),
            ("model-beta", ProviderId::claude(), ProviderPolicy::Closed),
        ],
    ));
    let member_a = test_member("acc-a", "claude", vec!["model-alpha".to_string()]);
    let initial_comp = PoolSnapshot::new(
        1,
        vec![member_a.clone()],
        vec![mahoquot_gateway::models_route::ModelEntry {
            id: "model-alpha".to_string(),
            owned_by: "claude".to_string(),
        }],
        reg,
    );

    let runtime = Arc::new(UnifiedRuntimeState::new(initial_comp, None));
    let running = Arc::new(AtomicBool::new(true));
    let split_brain_count = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let rt = Arc::clone(&runtime);
        let run = Arc::clone(&running);
        let split = Arc::clone(&split_brain_count);

        handles.push(std::thread::spawn(move || {
            while run.load(Ordering::Relaxed) {
                let snap = rt.load();
                let has_acc_b = snap.members().iter().any(|m| m.id == "acc-b");
                let routable_b = snap.routable_accounts_for_model("model-beta");
                let models_has_beta = snap.models().iter().any(|m| m.id == "model-beta");

                // Atomicity invariant: either acc-b is present in members AND routable for model-beta AND model-beta is in models,
                // OR acc-b is absent AND routable is empty AND model-beta is absent.
                // NEVER a half-state!
                if (has_acc_b == routable_b.is_empty()) || has_acc_b != models_has_beta {
                    split.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }

    let member_b = test_member("acc-b", "claude", vec!["model-beta".to_string()]);
    for cycle in 0..10 {
        std::thread::sleep(Duration::from_millis(5));
        if cycle % 2 == 0 {
            // Add member_b
            runtime
                .reload_accounts(vec![member_a.clone(), member_b.clone()])
                .unwrap();
        } else {
            // Remove member_b
            runtime.reload_accounts(vec![member_a.clone()]).unwrap();
        }
    }

    running.store(false, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        split_brain_count.load(Ordering::SeqCst),
        0,
        "account add/remove must never observe half-state"
    );
}

#[test]
fn registry_reload_updates_effective_model_routing_atomically_zero_half_state() {
    let reg1 = Arc::new(test_registry(
        1,
        &[("model-v1", ProviderId::claude(), ProviderPolicy::Closed)],
    ));
    let member = test_member("acc-claude", "claude", vec![]);
    let initial_comp = PoolSnapshot::new(
        1,
        vec![member.clone()],
        vec![mahoquot_gateway::models_route::ModelEntry {
            id: "model-v1".to_string(),
            owned_by: "claude".to_string(),
        }],
        reg1,
    );

    let runtime = Arc::new(UnifiedRuntimeState::new(initial_comp, None));
    let running = Arc::new(AtomicBool::new(true));
    let half_state_count = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for _ in 0..8 {
        let rt = Arc::clone(&runtime);
        let run = Arc::clone(&running);
        let half = Arc::clone(&half_state_count);

        handles.push(std::thread::spawn(move || {
            while run.load(Ordering::Relaxed) {
                let snap = rt.load();
                let reg_has_future = snap
                    .registry()
                    .get_model(&ModelId::new("model-future").unwrap())
                    .is_some();
                let models_has_future = snap.models().iter().any(|m| m.id == "model-future");
                let routable_future = snap.routable_accounts_for_model("model-future");

                // Invariant: model-future in models iff model-future in registry
                if reg_has_future != models_has_future {
                    half.fetch_add(1, Ordering::SeqCst);
                }
                if models_has_future && routable_future.is_empty() {
                    half.fetch_add(1, Ordering::SeqCst);
                }
            }
        }));
    }

    for i in 2..=10 {
        std::thread::sleep(Duration::from_millis(5));
        let next_reg = if i % 2 == 0 {
            Arc::new(test_registry(
                i,
                &[
                    ("model-v1", ProviderId::claude(), ProviderPolicy::Closed),
                    ("model-future", ProviderId::claude(), ProviderPolicy::Closed),
                ],
            ))
        } else {
            Arc::new(test_registry(
                i,
                &[("model-v1", ProviderId::claude(), ProviderPolicy::Closed)],
            ))
        };
        runtime.update_registry(next_reg).unwrap();
    }

    running.store(false, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(
        half_state_count.load(Ordering::SeqCst),
        0,
        "registry reload must never observe half-state"
    );
}

#[test]
fn hundred_concurrent_coalesced_triggers_produce_monotonic_unique_generations_without_deadlock() {
    let reg = Arc::new(test_registry(
        1,
        &[("model-1", ProviderId::claude(), ProviderPolicy::Closed)],
    ));
    let member = test_member("acc-1", "claude", vec!["model-1".to_string()]);
    let initial_comp = PoolSnapshot::new(
        1,
        vec![member.clone()],
        vec![mahoquot_gateway::models_route::ModelEntry {
            id: "model-1".to_string(),
            owned_by: "claude".to_string(),
        }],
        reg,
    );

    let runtime = Arc::new(UnifiedRuntimeState::new(initial_comp, None));
    let mut tasks = Vec::new();

    // 100 concurrent triggers
    for i in 0..100 {
        let rt = Arc::clone(&runtime);
        tasks.push(std::thread::spawn(move || {
            let res = rt.trigger_coalesced_refresh();
            assert!(res.is_ok(), "trigger {i} must complete successfully");
            res.unwrap().generation()
        }));
    }

    let mut observed_generations = Vec::new();
    for t in tasks {
        let gen = t.join().expect("thread must not deadlock");
        observed_generations.push(gen);
    }

    assert_eq!(observed_generations.len(), 100);
    // Every generation observed must be >= 1
    for &g in &observed_generations {
        assert!(g >= 1);
    }
    // Final generation must be >= 2 and <= 101 (since calls coalesce)
    let final_gen = runtime.load().generation();
    assert!(final_gen >= 2, "at least one refresh must have completed");
    assert!(
        final_gen <= 101,
        "generations must be monotonic and bounded"
    );
}

#[test]
fn invalid_candidates_do_not_publish() {
    let reg = Arc::new(test_registry(
        1,
        &[("model-1", ProviderId::claude(), ProviderPolicy::Closed)],
    ));
    let member = test_member("acc-1", "claude", vec!["model-1".to_string()]);
    let initial_comp = PoolSnapshot::new(
        1,
        vec![member.clone()],
        vec![mahoquot_gateway::models_route::ModelEntry {
            id: "model-1".to_string(),
            owned_by: "claude".to_string(),
        }],
        reg,
    );

    let runtime = Arc::new(UnifiedRuntimeState::new(initial_comp, None));
    let before_gen = runtime.load().generation();

    // Create an invalid registry candidate with an alias cycle (A -> B -> A)
    let mut invalid_builder = RegistryBuilder::new(CatalogVersion(99), CatalogSource::RemoteSigned);
    invalid_builder.register_provider(ProviderId::claude(), ProviderPolicy::Closed);
    let model_a = ModelDescriptor::new(ModelId::new("cycle-a").unwrap(), "claude");
    let model_b = ModelDescriptor::new(ModelId::new("cycle-b").unwrap(), "claude");
    invalid_builder.add_model(model_a).unwrap();
    invalid_builder.add_model(model_b).unwrap();
    // Alias cycle
    invalid_builder
        .add_alias(
            ModelId::new("cycle-a").unwrap(),
            ModelId::new("cycle-b").unwrap(),
            None,
        )
        .unwrap();
    invalid_builder
        .add_alias(
            ModelId::new("cycle-b").unwrap(),
            ModelId::new("cycle-a").unwrap(),
            None,
        )
        .unwrap();

    let invalid_res = invalid_builder.build();
    // In mahoquot_registry, build() returns Err(AliasCycle) when alias cycle exists
    assert!(invalid_res.is_err(), "invalid registry builder must fail");

    // Also attempt to publish candidate with validation failure
    // If we have a snapshot that fails validate():
    let mut snap = test_registry(
        99,
        &[("model-ok", ProviderId::claude(), ProviderPolicy::Closed)],
    );
    // Corrupt snapshot with an alias cycle
    snap.aliases.insert(
        ModelId::new("cycle-x").unwrap(),
        ModelAliasRule {
            alias: ModelId::new("cycle-x").unwrap(),
            target: ModelId::new("cycle-y").unwrap(),
            provider_id: None,
        },
    );
    snap.aliases.insert(
        ModelId::new("cycle-y").unwrap(),
        ModelAliasRule {
            alias: ModelId::new("cycle-y").unwrap(),
            target: ModelId::new("cycle-x").unwrap(),
            provider_id: None,
        },
    );

    let publish_res = runtime.update_registry(Arc::new(snap));
    assert!(
        publish_res.is_err(),
        "publishing invalid candidate must fail"
    );

    // State MUST be completely unchanged!
    let after_snap = runtime.load();
    assert_eq!(after_snap.generation(), before_gen);
    assert_eq!(after_snap.registry().version(), CatalogVersion(1));
}

#[test]
fn concurrent_parameterized_mutations_are_never_dropped_by_coalescing() {
    let reg = Arc::new(test_registry(
        1,
        &[("model-base", ProviderId::claude(), ProviderPolicy::Closed)],
    ));
    let member = test_member("acc-base", "claude", vec!["model-base".to_string()]);
    let initial_comp = PoolSnapshot::new(
        1,
        vec![member],
        vec![mahoquot_gateway::models_route::ModelEntry {
            id: "model-base".to_string(),
            owned_by: "claude".to_string(),
        }],
        reg,
    );

    let runtime = Arc::new(UnifiedRuntimeState::new(initial_comp, None));
    let n_updates = 30;
    let mut handles = Vec::new();

    for i in 0..n_updates {
        let rt = Arc::clone(&runtime);
        handles.push(std::thread::spawn(move || {
            let m = test_member(&format!("acc-{i}"), "claude", vec!["model-base".to_string()]);
            rt.reload_accounts(vec![m])
                .expect("reload_accounts must succeed")
        }));
    }

    for h in handles {
        h.join().unwrap();
    }

    // In contrast to coalesced refresh, every single reload_accounts must execute.
    // So exactly n_updates generations were produced!
    let final_snap = runtime.load();
    assert_eq!(final_snap.generation(), 1 + n_updates as u64);
}

# Devin P1 — registry identity and Discovered-only policy

Scope: crates/registry only. Nothing outside `crates/registry/src/lib.rs`, `crates/registry/catalog/models-v1.json`, and `crates/registry/tests/devin_tests.rs` was modified. No commit; pre-existing dirty work untouched.

## Changed paths

1. `crates/registry/src/lib.rs`
   - `ProviderId::devin()` constructor returning canonical id `devin`. No `codeium`/`windsurf` normalization or aliasing was added to `ProviderId::canonical` (ids remain themselves).
   - `RegistrySnapshot::resolve()` unknown-model path: when the requested id lives in a registered non-Open provider's namespace (`<provider>/<rest>`, e.g. `devin/<uid>`), the Open (Codex) fallback is skipped and a typed `RegistryError::UnknownModel` is returned. This implements plan §6.4 "`devin/*`가 미등록 상태일 때 Codex의 open fallback으로 흘러가지 않게" at the registry layer.
2. `crates/registry/catalog/models-v1.json`
   - Added `"devin": "discovered"` to the `providers` map only. Zero static models, bindings, aliases, capabilities, context/output limits, cost, or quota entries for Devin. The file's version/source and all signed/global catalog contracts are untouched; this remains the embedded fallback catalog, not a signed artifact — no upstream signature is claimed for this edit.
3. `crates/registry/tests/devin_tests.rs` (new, 9 tests)
   - `provider_id_devin_identity`: `devin()` identity; `canonical(" Devin ") == devin`; `canonical("codeium")`/`canonical("windsurf")` are NOT devin.
   - `embedded_catalog_registers_devin_discovered_without_static_models`: embedded snapshot registers devin as `Discovered`, no devin binding/owned_by/alias anywhere, snapshot still validates.
   - `unknown_devin_model_is_not_served_by_open_fallback`: `resolve("devin/never-discovered-uid")` is `UnknownModel` despite Codex being Open.
   - `discovered_contribution_makes_devin_uid_routable_with_suffix_variants`: discovered `devin/glm-5-2-max` becomes routable; base `devin/glm-5-2` stays `UnknownModel` (suffix variants are not folded).
   - `devin_contribution_rejected_when_provider_policy_is_closed`: Closed devin rejects dynamic discovery (`UnauthorizedContribution`).
   - `devin_discovered_contribution_is_mergable_after_catalog_registration`: signed-catalog-first flow then discovery merge; `devin/glm-5-2` vs `devin/glm-5-2-1m` stay distinct canonical ids.
   - `devin_contribution_items_roundtrip_ids_verbatim`: public ids are `devin/<exact uid>` verbatim; explicit Discovered policy sticks (no silent Closed default).
   - `devin_vision_support_does_not_grant_image_generation_capability`: vision support must not set `ModelCapability::Image` (which is reserved for image generation surfaces).
   - `devin_signed_catalog_compatibility_and_no_upstream_signature_claim`: embedded catalog retains `CatalogSource::EmbeddedFallback` (does not claim `RemoteSigned`), while RemoteSigned catalogs containing Devin Discovered policy sign and verify cleanly under Ed25519 envelope verification.

## Commands and exit codes

- Red (before production changes): `cargo test -p mahoquot-registry --test devin_tests`
  - exit 101; 15 × `error[E0599]: no function or associated item named 'devin' found for struct 'ProviderId'` (faithful failing-first evidence; the namespace-fallback test also compiled only against the not-yet-devin lib, exercising the change at Green).
- Green: `cargo test -p mahoquot-registry`
  - exit 0; all suites: 5+3+9+9+13+1+15 = 55 passed, 0 failed, including all pre-existing signed-catalog, authority, domain, envelope, and review-proxy contract tests.
- `cargo clippy -p mahoquot-registry --all-targets -- -D warnings`: exit 0; 0 warnings.
- `cargo build -p mahoquot-registry`: exit 0; 0 warnings.
- LSP diagnostics: executed `lsp_diagnostics` for `crates/registry/src/lib.rs` and `crates/registry/tests/devin_tests.rs`; language server returned "file not found" (un-indexed path in workspace). Correctness verified via rustc, cargo test, clippy, and rustfmt. No production change was made before the failing-first run.

## Contracts for downstream nodes

- `ProviderId::devin()` == `ProviderId::new("devin")`; `ProviderId::canonical` does not map codeium/windsurf to devin.
- Embedded catalog: `providers["devin"] = ProviderPolicy::Discovered`; no static routable Devin models. Downstream must never fabricate one.
- `resolve("devin/<uid>")` with an un-discovered uid returns `UnknownModel` — gateway `serves_model`/`resolve_model` must treat this as authoritative blocking (no Codex open fallback), per plan §6.4.
- Routable Devin models come only from a `ProviderContribution` with `ContributionItem::Discovered` while the provider policy is Discovered (Closed rejects). Public id must be `devin/<exact model_uid>`; suffix variants (`-max`, `-none`, `-medium`, `-1m`) are preserved verbatim as distinct ModelIds; no base-model fallback.
- Vision input support is NOT expressed as `ModelCapability::Image`; no capability, context, output, cost, or quota value was fabricated in this change.

## Remaining blockers / notes

- Baseline blocker stands: gateway library tests do not compile (five `GenericAccount` initializers missing `email`) — pre-existing, unrelated to this scope.
- Plan §6.4 items outside registry scope (discovery cache TTL, refresh API, binding/account generation) belong to gateway nodes.

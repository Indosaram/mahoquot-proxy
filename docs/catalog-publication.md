# Model Catalog Authoring, Signing, and Publication Workflow

This document specifies the operational lifecycle, cryptographic distribution architecture, validation rules, operator tooling, and failure recovery procedures for the Mahoquot Unified Runtime Model Registry (`mahoquot-registry`).

---

## 1. Architecture & Lifecycle

The model catalog follows an **offline-first, cryptographic precedence** lifecycle:

```
[ Embedded Fallback Catalog ]   (compiled into binary via include_bytes!)
            │
            ▼ (overridden if valid LKG exists with version >= embedded)
[ Last-Known-Good (LKG) Disk Cache ]  (~/.mahoquot/cache/models-v1.signed.json)
            │
            ▼ (background HTTP fetch with conditional GET / ETag)
[ Remote Signed Catalog Envelope ]  (published on orphan branch `model-catalog-v1`)
            │
            ▼ (applied dynamically during composition)
[ Local Settings Overrides & Aliases ] (config.yaml: oauth-model-alias, oauth-excluded-models, model-catalog)
```

### Lifecycle Phases:
1. **Embedded Fallback at Boot**: At gateway startup, the embedded catalog snapshot (`crates/registry/catalog/models-v1.json`) is initialized synchronously to ensure immediate service readiness with zero disk or network dependencies.
2. **LKG Cache Evaluation**: The gateway inspects the local disk cache (`~/.mahoquot/cache/models-v1.signed.json`). If present, syntactically valid, authenticated by a trusted public key in the keyring, and with `catalog_version >= embedded.version`, the LKG snapshot replaces the embedded snapshot.
3. **Background Remote Refresh**: After listener readiness, the background `CatalogManager` periodically polls GitHub raw content URLs using conditional HTTP headers (`If-None-Match` / `ETag`). New versions are downloaded, detached signatures verified against the public keyring, domain invariants validated, and upon success:
   - The verified package is atomically saved to the LKG cache file (`0600` permissions via atomic rename).
   - An immutable `PoolSnapshot` generation is composed and published via lock-free `ArcSwap`.
4. **Offline Resilience**: If network connectivity fails, remote endpoints return 5xx/404, or the remote signature is invalid/tampered, the gateway remains fully operational using the active LKG cache or embedded fallback without service interruption.

---

## 2. Source Precedence & Authority

When composing the active model registry snapshot, sources are merged deterministically in strict ascending order of precedence:

| Precedence | Source Level | `CatalogSource` | Description |
|:---:|---|---|---|
| **1 (Lowest)** | Embedded Fallback | `embedded_fallback` | Static artifact baked into binary at compile time. |
| **2** | LKG Cache | `lkg_cache` | Last-Known-Good verified signed package persisted on local disk. |
| **3** | Remote Signed | `remote_signed` | Verified signed catalog fetched from the approved GitHub branch. |
| **4** | Authoritative Discovery | `discovered` | Live discovery from providers with explicit discovery adapters (e.g. generic `/models`). |
| **5 (Highest)** | Local Override | `local_override` | Operator overrides from `config.yaml` (`oauth-model-alias`, `oauth-excluded-models`, `custom-models`). |

### Authority Masks
Each provider binding carries an `AuthorityMask` controlling which fields it is permitted to contribute:
- `models`: Authority to contribute model IDs.
- `capabilities`: Authority to declare chat, image, video, realtime, tools, etc.
- `aliases`: Authority to register model alias pointers.
- `prefixes`: Authority to define routing prefix mappings.
- `upstream_id`: Authority to rewrite the upstream model identifier.

Partial discovery contributions cannot erase fields outside their authority mask (for example, sparse OpenAI `/models` listing contributes model IDs but cannot strip known image/chat capabilities defined in the catalog).

---

## 3. Provider Policy Matrix

Admission to the registry is governed by explicit provider policies:

| Policy | Providers | Dynamic Discovery Behavior | Codex Negative Space Fallback |
|---|---|---|---|
| **`Closed`** | `antigravity`, `claude`, `cursor`, `kiro`, `vertex`, `zcode` | **Prohibited.** Dynamic contributions are rejected with `UnauthorizedContribution`. Model lists, capabilities, and aliases must be declared via signed catalog or local operator overrides. | Ineligible. Cannot claim unknown arbitrary models. |
| **`Discovered`** | Generic OpenAI-compatible accounts with explicit discovery enabled | **Allowed within authority mask.** Discovered model IDs are bound to the discovering account, but cannot overwrite signed catalog capabilities or alias rules. | Ineligible. |
| **`Open`** | `codex` (OpenAI ChatGPT / Codex) | **Open Fallback.** Operates across negative space: any unmapped model request that is NOT claimed by a Closed or Discovered provider is routed to Codex fallback. | **Yes.** Codex acts as the universal open fallback for standard LLM IDs, but cannot steal models claimed by closed providers. |

---

## 4. Distribution Architecture & Pinned URLs

Signed catalog files are published exclusively to the orphan branch `model-catalog-v1` in the `Indosaram/mahoquot-proxy` repository.

### Publication Invariant: Independent Release
Publishing a new model catalog **does NOT** create a Git release tag (`v*`), build gateway binaries, or require restarting running gateways. Running gateways detect new signed catalogs automatically on their background refresh cycle (default: 3600s) or via the management refresh API.

### Raw Pinned URLs
- **Catalog Payload (canonical JSON)**:  
  `https://raw.githubusercontent.com/Indosaram/mahoquot-proxy/model-catalog-v1/models-v1.json`
- **Detached Ed25519 Signature Envelope**:  
  `https://raw.githubusercontent.com/Indosaram/mahoquot-proxy/model-catalog-v1/models-v1.json.sig`
- **SHA-256 Checksum**:  
  `https://raw.githubusercontent.com/Indosaram/mahoquot-proxy/model-catalog-v1/models-v1.json.sha256`

---

## 5. Catalog JSON Schemas & Validation Invariants

Schemas are versioned and stored under `crates/registry/schema/` and `docs/schemas/`:
- **Catalog Manifest**: `docs/schemas/catalog-v1.schema.json`
- **Signature Envelope**: `docs/schemas/envelope-v1.schema.json`

### Signature Envelope Format (`models-v1.json.sig`)
```json
{
  "schema_version": 1,
  "catalog_version": 42,
  "key_id": "mahoquot-prod-2026-v1",
  "generated_at": 1788390000,
  "expires_at": null,
  "signature": "DqvK9eApOLgTDPHoKK1sXrgZoxFJQtIfZ7fJin7tGxm5mSKU2LmwqLfHgoDp1h6Fbbl1G3JR4mCVzu9uFoOIDg=="
}
```

### Verification & Anti-Downgrade Invariants
1. **RFC 8785 Canonicalization**: The payload JSON must be formatted in deterministic RFC 8785 byte order. Any extra whitespace, reordered keys, or float variations invalidate the Ed25519 signature.
2. **Keyring Trust**: The `key_id` must match a trusted public key in the gateway's embedded keyring.
3. **Anti-Downgrade Rule**: An incoming catalog is rejected if `catalog_version <= max(active_version, lkg_version)`. Catalog versions are strictly monotonic unsigned integers.
4. **Clock Skew & Expiration**:
   - `generated_at > now + allowed_clock_skew_secs` (default 300s): Rejected (`FutureTimestamp`).
   - `expires_at < now` (if set): Rejected (`Expired`).
5. **Domain Invariants**:
   - Provider IDs and Model IDs must be non-empty and contain no whitespace or control characters.
   - Alias depth must not exceed 10 (`MAX_ALIAS_DEPTH`). Alias cycles and dangling alias targets are rejected.
   - Duplicate bindings with conflicting properties without proper precedence are rejected.
   - Catalogs with zero fallback-routable bindings are rejected.

---

## 6. Key Custody, Rotation, and Revocation

### Custody Principles
- Private signing keys **NEVER** enter source code repositories, commit histories, PR logs, or gateway binaries.
- Production signing private keys are stored exclusively as protected secrets in the GitHub Actions `catalog-publication` environment (`MAHOQUOT_MODEL_CATALOG_ED25519_PRIVATE_KEY`).
- PR validation workflows only run linting, schema validation, and test-key verification. They do not have access to the production environment.

### Embedded Keyring
The gateway binary contains an embedded public keyring (`Keyring::embedded_default()`):
- `mahoquot-prod-2026-v1`: Production catalog publisher key.
- `test-ed25519-v1`: Development and CI fixture key.

### Key Rotation Procedure (Overlapping Trust)
To rotate a signing key without service interruption:
1. **Add New Public Key**: In a new gateway release, add the upcoming public key (e.g. `mahoquot-prod-2027-v1`) to `Keyring::embedded_default()` alongside the existing key.
2. **Deploy Gateway Update**: Distribute the gateway update so client instances trust both old and new keys.
3. **Switch Publisher Key**: Update GitHub Actions environment secret `MAHOQUOT_MODEL_CATALOG_ED25519_PRIVATE_KEY` and set `KEY_ID` to `mahoquot-prod-2027-v1`.
4. **Retire Old Key**: After a transition window, remove the retired public key in a subsequent gateway release.

### Key Revocation
If a private signing key is compromised:
- Immediately release a patch gateway version removing the compromised `key_id` from the embedded keyring.
- Gateways booting or refreshing with the revoked key will reject the signature and fall back safely to LKG or embedded catalogs.

---

## 7. Cache Location & Offline Persistence

### Cache Location & Permissions
- **Default Location**: `~/.mahoquot/cache/models-v1.signed.json`
- **Environment Override**: `MAHOQUOT_CACHE_DIR=/custom/path` (resolves to `$MAHOQUOT_CACHE_DIR/models-v1.signed.json`).
- **File Permissions**: Must be written with POSIX mode `0600` (read/write by owner only).

### Atomic Persistence & Recovery
To prevent partial writes or corruption caused by power loss or crashes:
1. The serialized package (`SignedCatalogPackage` containing envelope and canonical payload) is written to a unique hidden temporary file:  
   `~/.mahoquot/cache/.models-v1.signed.json.tmp.<PID>.<NONCE>`
2. Permissions are explicitly set to `0600`.
3. File contents are synchronized to disk via `fsync` (`sync_all()`).
4. The temporary file is atomically renamed over `models-v1.signed.json` (`rename(2)` atomic semantics).
5. **Corrupted Cache Fallback**: If the cache file is truncated or corrupted, the loader logs a warning, marks telemetry status as `stale`, and falls back to the embedded catalog without crashing.

---

## 8. Local Overrides, Exclusions, and Aliases

Operators can configure local routing rules and aliases in `config.yaml`. These rules are validated transactionally at startup and settings modification:

```yaml
model-catalog:
  refresh-enabled: true
  url: "https://raw.githubusercontent.com/Indosaram/mahoquot-proxy/model-catalog-v1/models-v1.json"
  signature-url: "https://raw.githubusercontent.com/Indosaram/mahoquot-proxy/model-catalog-v1/models-v1.json.sig"
  refresh-interval-secs: 3600
  allowed-blackouts: []
  custom-models: []

oauth-model-alias:
  antigravity:
    - name: "gemini-2.5-pro"
      alias: "gemini-pro"
  claude:
    - name: "claude-3-7-sonnet-20250219"
      alias: "claude-sonnet-latest"

oauth-excluded-models:
  antigravity:
    - "gemini-1.0-pro"
```

Validation rejects circular aliases, excessive alias depths, unknown alias targets, and exclusions that cause a total provider blackout without explicit override.

---

## 9. Management Endpoints

The gateway exposes authenticated management endpoints under `/v0/management/model-registry` (requires `Authorization: Bearer <API_KEY>`):

### `GET /v0/management/model-registry`
Returns safe observable status of the runtime model registry:
```json
{
  "source": "remote_signed",
  "catalog-version": 42,
  "generation": 3,
  "generated-at": 1788390000,
  "loaded-at": 1788390100,
  "stale": false,
  "last-refresh": {
    "outcome": "success",
    "attempted-at": 1788390100,
    "duration-ms": 142,
    "rejection-reason": null
  },
  "provider-count": 14,
  "model-count": 102,
  "refresh-in-flight": false
}
```

*Note: The response never exposes credentials, signing private keys, signatures, local cache filesystem paths, or user account IDs.*

### `POST /v0/management/model-registry`
Triggers an immediate background refresh:
- Returns `202 Accepted` immediately without blocking on network I/O.
- Concurrent requests are coalesced (`coalesced: true`).

---

## 10. Telemetry & Metrics

The gateway exposes Prometheus metrics at `/metrics`:

| Metric Name | Type | Labels | Description |
|---|---|---|---|
| `mahoquot_model_registry_refresh_attempts_total` | Counter | None | Total background and manual refresh attempts. |
| `mahoquot_model_registry_refresh_outcomes_total` | Counter | `outcome="success" \| "error"` | Count of refresh outcomes. |
| `mahoquot_model_registry_refresh_rejections_total` | Counter | `reason="coalesced" \| ...` | Trigger rejections (e.g. coalesced requests). |
| `mahoquot_model_registry_refresh_duration_milliseconds_total` | Counter | None | Cumulative refresh duration in milliseconds. |
| `mahoquot_model_registry_cache_source_total` | Counter | `source="embedded_fallback" \| "lkg_cache" \| "remote_signed" \| "discovered" \| "local_override"` | Successful registry activations by source. |

---

## 11. Exact Operator Commands

Operators and CI/CD pipelines use `tools/model-catalog` (`mahoquot-model-catalog`) for all catalog operations:

### 1. Validate Catalog
Validate JSON syntax, JSON schema conformance, and domain invariants (alias chains, provider policies, routable bindings):
```bash
cargo run -p mahoquot-model-catalog -- validate crates/registry/catalog/models-v1.json
```

### 2. Canonicalize & Sign
Canonicalize into RFC 8785 byte order and generate a detached Ed25519 signature envelope:
```bash
cargo run -p mahoquot-model-catalog -- sign \
  --key-file tests/fixtures/test-ed25519.key \
  --input crates/registry/catalog/models-v1.json \
  --output target/models-v1.json \
  --signature target/models-v1.json.sig \
  --key-id test-ed25519-v1
```

### 3. Verify Signed Envelope
Verify that the canonical payload matches the detached signature envelope:
```bash
cargo run -p mahoquot-model-catalog -- verify \
  --input target/models-v1.json \
  --signature target/models-v1.json.sig
```

### 4. Dry-Run Publication Workflow
Perform an end-to-end local dry run using the verification script:
```bash
bash scripts/verify-catalog.sh
```

### 5. Inspect Local Disk Cache
Inspect the local gateway cache and verify permissions:
```bash
ls -la ~/.mahoquot/cache/models-v1.signed.json
# Ensure permissions are -rw------- (0600)
```

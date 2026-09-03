# Test Ed25519 Signing Keypair

This directory contains an ephemeral, non-production Ed25519 keypair used strictly for local development, unit/integration testing, and CI verification workflows.

**DO NOT USE THESE KEYS IN PRODUCTION.**

## Key Details
- **Key ID**: `test-ed25519-v1`
- **Private Key (`test-ed25519.key`)**: `7961298453401768403920194857392018475930291847583920194857692834` (Hex-encoded 32-byte Ed25519 seed)
- **Public Key (`test-ed25519.pub`)**: `5b94a233671dfc90c58a7aafbde394bb25b35f66b782d6f21db706613b74adac` (Hex-encoded 32-byte Ed25519 verifying key)

## Usage in Development and CI
To sign a catalog with this key:
```bash
cargo run -p mahoquot-model-catalog -- sign \
  --key-file tests/fixtures/test-ed25519.key \
  --input crates/registry/catalog/models-v1.json \
  --output target/models-v1.json \
  --signature target/models-v1.json.sig
```

To verify the generated envelope:
```bash
cargo run -p mahoquot-model-catalog -- verify \
  --input target/models-v1.json \
  --signature target/models-v1.json.sig
```

In CI environments, the private key can also be provided via the `MAHOQUOT_MODEL_CATALOG_ED25519_PRIVATE_KEY` environment variable.

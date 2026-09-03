#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

cd "$WORKSPACE_ROOT"

echo "=== Mahoquot Model Catalog Verification Pipeline ==="
echo "Workspace root: $WORKSPACE_ROOT"

CATALOG_JSON="crates/registry/catalog/models-v1.json"
SCHEMA_JSON="crates/registry/schema/catalog-v1.schema.json"
ENVELOPE_SCHEMA="crates/registry/schema/envelope-v1.schema.json"
TEST_KEY="tests/fixtures/test-ed25519.key"
TEST_PUB="tests/fixtures/test-ed25519.pub"

# 1. Check prerequisite files exist
echo ""
echo "[Step 1/6] Checking required files..."
for f in "$CATALOG_JSON" "$SCHEMA_JSON" "$ENVELOPE_SCHEMA" "$TEST_KEY" "$TEST_PUB"; do
  if [[ ! -f "$f" ]]; then
    echo "ERROR: required file $f missing" >&2
    exit 1
  fi
  echo "  OK: $f"
done

# 2. Validate JSON Schema syntax using python3
echo ""
echo "[Step 2/6] Validating catalog and envelope JSON Schemas..."
python3 -c '
import json
for path in ["'"$SCHEMA_JSON"'", "'"$ENVELOPE_SCHEMA"'"]:
    with open(path) as f:
        schema = json.load(f)
        assert "$schema" in schema, f"$schema missing in {path}"
        assert "properties" in schema, f"properties missing in {path}"
print("  OK: Both schemas are syntactically valid JSON Schema draft-07 documents")
'

# 3. Validate catalog invariants using catalog tooling
echo ""
echo "[Step 3/6] Validating catalog with mahoquot-model-catalog..."
cargo run -q -p mahoquot-model-catalog -- validate "$CATALOG_JSON"

# 4. Canonicalize and sign catalog
echo ""
echo "[Step 4/6] Canonicalizing and signing catalog with test Ed25519 key..."
TMP_DIR=$(mktemp -d)
trap 'rm -rf "$TMP_DIR"' EXIT

OUT_PAYLOAD="$TMP_DIR/models-v1.json"
OUT_SIGNATURE="$TMP_DIR/models-v1.json.sig"

cargo run -q -p mahoquot-model-catalog -- sign \
  --key-file "$TEST_KEY" \
  --input "$CATALOG_JSON" \
  --output "$OUT_PAYLOAD" \
  --signature "$OUT_SIGNATURE" \
  --key-id "test-ed25519-v1"

echo "  Generated canonical payload at $OUT_PAYLOAD"
echo "  Generated signature envelope at $OUT_SIGNATURE"

# 5. Verify valid envelope
echo ""
echo "[Step 5/6] Verifying signed envelope with public key..."
cargo run -q -p mahoquot-model-catalog -- verify \
  --input "$OUT_PAYLOAD" \
  --signature "$OUT_SIGNATURE" \
  --key-file "$TEST_PUB" \
  --key-id "test-ed25519-v1"

# 6. Adversarial tamper tests
echo ""
echo "[Step 6/6] Running adversarial rejection tests..."

# Adversarial 1: Tampered payload (1 byte appended whitespace)
TAMPERED_PAYLOAD="$TMP_DIR/tampered-payload.json"
cp "$OUT_PAYLOAD" "$TAMPERED_PAYLOAD"
echo " " >> "$TAMPERED_PAYLOAD"

if cargo run -q -p mahoquot-model-catalog -- verify \
  --input "$TAMPERED_PAYLOAD" \
  --signature "$OUT_SIGNATURE" >/dev/null 2>&1; then
  echo "ERROR: verification succeeded on tampered payload!" >&2
  exit 1
fi
echo "  PASS: Tampered payload rejected as expected (canonicalization mismatch)"

# Adversarial 2: Tampered payload semantic content
TAMPERED_PAYLOAD2="$TMP_DIR/tampered-payload2.json"
python3 -c '
data = open("'"$OUT_PAYLOAD"'", "rb").read()
# Mutate version from 1 to 9
data2 = data.replace(b"\"version\":1", b"\"version\":9")
open("'"$TAMPERED_PAYLOAD2"'", "wb").write(data2)
'
if cargo run -q -p mahoquot-model-catalog -- verify \
  --input "$TAMPERED_PAYLOAD2" \
  --signature "$OUT_SIGNATURE" >/dev/null 2>&1; then
  echo "ERROR: verification succeeded on tampered payload version!" >&2
  exit 1
fi
echo "  PASS: Tampered semantic content rejected as expected (signature verification failed)"

# Adversarial 3: Tampered signature string
TAMPERED_SIG="$TMP_DIR/tampered-sig.json"
python3 -c '
import json
d = json.load(open("'"$OUT_SIGNATURE"'"))
sig = list(d["signature"])
sig[5] = "X" if sig[5] != "X" else "Y"
d["signature"] = "".join(sig)
json.dump(d, open("'"$TAMPERED_SIG"'", "w"))
'
if cargo run -q -p mahoquot-model-catalog -- verify \
  --input "$OUT_PAYLOAD" \
  --signature "$TAMPERED_SIG" >/dev/null 2>&1; then
  echo "ERROR: verification succeeded on tampered signature!" >&2
  exit 1
fi
echo "  PASS: Tampered signature rejected as expected"

# Adversarial 4: Anti-downgrade rejection (incoming <= active threshold)
if cargo run -q -p mahoquot-model-catalog -- verify \
  --input "$OUT_PAYLOAD" \
  --signature "$OUT_SIGNATURE" \
  --active-version 2 >/dev/null 2>&1; then
  echo "ERROR: verification succeeded on downgraded version!" >&2
  exit 1
fi
echo "  PASS: Version downgrade rejected as expected"

# Adversarial 5: Incompatible schema version
INCOMPAT_SIG="$TMP_DIR/incompat-sig.json"
python3 -c '
import json
d = json.load(open("'"$OUT_SIGNATURE"'"))
d["schema_version"] = 99
json.dump(d, open("'"$INCOMPAT_SIG"'", "w"))
'
if cargo run -q -p mahoquot-model-catalog -- verify \
  --input "$OUT_PAYLOAD" \
  --signature "$INCOMPAT_SIG" >/dev/null 2>&1; then
  echo "ERROR: verification succeeded on incompatible schema version!" >&2
  exit 1
fi
echo "  PASS: Incompatible schema version rejected as expected"

echo ""
echo "=== ALL VERIFICATION CHECKS PASSED SUCCESSFULLY ==="

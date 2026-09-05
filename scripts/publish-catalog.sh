#!/usr/bin/env bash
set -euo pipefail

TOOL=${1:?catalog tool path required}
INPUT=${2:?catalog input path required}
KEY_ID=${3:?signing key ID required}
shift 3
BRANCH=model-catalog-v1
PUBLISH_DIR=$(mktemp -d "${TMPDIR:-/tmp}/catalog-publish.XXXXXX")
trap 'rm -rf "$PUBLISH_DIR"' EXIT

# An empty successful listing means absent; transport/auth errors must fail.
REMOTE_REF=$(git ls-remote --heads origin "refs/heads/$BRANCH")
PARENTS=()
if [[ -n "$REMOTE_REF" ]]; then
  git fetch --no-tags origin "refs/heads/$BRANCH"
  PARENT=$(git rev-parse FETCH_HEAD)
  PARENTS=(-p "$PARENT")
  git show "$PARENT:models-v1.json" > "$PUBLISH_DIR/previous.json"
  python3 - "$PUBLISH_DIR/previous.json" "$INPUT" <<'PY'
import json
import sys
previous, incoming = [json.load(open(path))["version"] for path in sys.argv[1:]]
if type(previous) is not int or type(incoming) is not int or incoming <= previous:
    sys.exit("catalog version must increase from the published version")
PY
fi

"$TOOL" sign --input "$INPUT" --output "$PUBLISH_DIR/models-v1.json" \
  --signature "$PUBLISH_DIR/models-v1.json.sig" --key-id "$KEY_ID"
"$TOOL" verify --input "$PUBLISH_DIR/models-v1.json" \
  --signature "$PUBLISH_DIR/models-v1.json.sig" "$@"
(cd "$PUBLISH_DIR"
 if command -v sha256sum >/dev/null 2>&1; then
   sha256sum models-v1.json > models-v1.json.sha256
 else
   shasum -a 256 models-v1.json > models-v1.json.sha256
 fi)

# A private index leaves the source checkout and its index untouched.
export GIT_INDEX_FILE="$PUBLISH_DIR/index"
git read-tree --empty
for FILE in models-v1.json models-v1.json.sig models-v1.json.sha256; do
  OBJECT=$(git hash-object -w "$PUBLISH_DIR/$FILE")
  git update-index --add --cacheinfo "100644,$OBJECT,$FILE"
done
TREE=$(git write-tree)
VERSION=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$PUBLISH_DIR/models-v1.json")
COMMIT=$(git -c user.name='github-actions[bot]' -c user.email='github-actions[bot]@users.noreply.github.com' \
  commit-tree "$TREE" "${PARENTS[@]}" -m "publish(catalog): version $VERSION [skip ci]")
git push origin "$COMMIT:refs/heads/$BRANCH"

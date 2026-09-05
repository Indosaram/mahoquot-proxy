#!/usr/bin/env bash
set -euo pipefail

TARGET=${1:?target triple required}
PLATFORM=${2:?runner OS required}
BIN_PATH="target/$TARGET/release/mahoquot-gateway"
ARCHIVE="mahoquot-gateway-$TARGET"
if [[ "$PLATFORM" == Windows ]]; then
  BIN_PATH+=.exe
  ARCHIVE+=.zip
else
  ARCHIVE+=.tar.gz
fi
test -f "$BIN_PATH"
# Never update an existing zip and accidentally retain obsolete members.
rm -f "$ARCHIVE"
if [[ "$PLATFORM" == Windows ]]; then
  if command -v 7z >/dev/null 2>&1; then
    (cd "$(dirname "$BIN_PATH")" && 7z a "$OLDPWD/$ARCHIVE" "$(basename "$BIN_PATH")")
  else
    zip -j "$ARCHIVE" "$BIN_PATH"
  fi
else
  COPYFILE_DISABLE=1 tar -czf "$ARCHIVE" -C "$(dirname "$BIN_PATH")" mahoquot-gateway
fi
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$ARCHIVE" > "$ARCHIVE.sha256"
else
  shasum -a 256 "$ARCHIVE" > "$ARCHIVE.sha256"
fi
if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  printf 'archive=%s\nchecksum=%s.sha256\n' "$ARCHIVE" "$ARCHIVE" >> "$GITHUB_OUTPUT"
fi

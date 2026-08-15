#!/usr/bin/env bash
set -euo pipefail

: "${TARGET:?TARGET is required}"
: "${VERSION:?VERSION is required}"
BIN_NAME="${BIN_NAME:-mindreader}"
OUT_DIR="${OUT_DIR:-dist}"
ARCHIVE_PATH="${OUT_DIR}/${BIN_NAME}-v${VERSION}-${TARGET}.tar.gz"
CHECKSUM_PATH="${ARCHIVE_PATH}.sha256"

[[ -f "$ARCHIVE_PATH" ]] || { echo "Archive not found: $ARCHIVE_PATH" >&2; exit 1; }
[[ -f "$CHECKSUM_PATH" ]] || { echo "Checksum not found: $CHECKSUM_PATH" >&2; exit 1; }

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$OUT_DIR" && sha256sum --check "$(basename "$CHECKSUM_PATH")")
else
  expected="$(awk '{print $1}' "$CHECKSUM_PATH")"
  actual="$(shasum -a 256 "$ARCHIVE_PATH" | awk '{print $1}')"
  [[ "$actual" == "$expected" ]] || { echo "Checksum mismatch for $ARCHIVE_PATH" >&2; exit 1; }
fi

smoke_dir="$(mktemp -d)"
trap 'rm -rf "$smoke_dir"' EXIT
tar -xzf "$ARCHIVE_PATH" -C "$smoke_dir"
BIN_PATH="${smoke_dir}/${BIN_NAME}"
[[ -x "$BIN_PATH" ]] || { echo "Binary is not executable: $BIN_PATH" >&2; exit 1; }
"$BIN_PATH" --version
"$BIN_PATH" --help >/dev/null

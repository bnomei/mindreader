#!/usr/bin/env bash
set -euo pipefail

: "${TARGET:?TARGET is required}"
: "${VERSION:?VERSION is required}"
BIN_NAME="${BIN_NAME:-mindreader}"
OUT_DIR="${OUT_DIR:-dist}"
ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

TARGET_DIR="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p' | head -n 1)"
TARGET_DIR="${TARGET_DIR:-target}"
BIN_PATH="${TARGET_DIR}/${TARGET}/release/${BIN_NAME}"

if [[ ! -f "$BIN_PATH" ]]; then
  echo "Binary not found: $BIN_PATH" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
ARCHIVE_NAME="${BIN_NAME}-v${VERSION}-${TARGET}.tar.gz"
tar -czf "${OUT_DIR}/${ARCHIVE_NAME}" \
  -C "${TARGET_DIR}/${TARGET}/release" "$BIN_NAME" \
  -C "$ROOT_DIR" LICENSE

if command -v sha256sum >/dev/null 2>&1; then
  (cd "$OUT_DIR" && sha256sum "$ARCHIVE_NAME" > "$ARCHIVE_NAME.sha256")
else
  (cd "$OUT_DIR" && shasum -a 256 "$ARCHIVE_NAME" > "$ARCHIVE_NAME.sha256")
fi

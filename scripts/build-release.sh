#!/usr/bin/env bash
set -euo pipefail

: "${TARGET:?TARGET is required}"
PACKAGE_NAME="${PACKAGE_NAME:-mindreader}"
BUILD_ARGS=(build --locked --release -p "$PACKAGE_NAME" --bin "$PACKAGE_NAME" --target "$TARGET")

if [[ "$TARGET" == *-unknown-linux-musl ]] && command -v cross >/dev/null 2>&1; then
  cross "${BUILD_ARGS[@]}"
else
  cargo "${BUILD_ARGS[@]}"
fi

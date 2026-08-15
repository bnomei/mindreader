#!/bin/sh
set -eu

bin=mindreader
repo=bnomei/mindreader

die() { printf 'error: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
Install Mindreader from a checksummed GitHub Release archive.

Usage:
  curl -fsSL https://raw.githubusercontent.com/bnomei/mindreader/main/scripts/install.sh | sh

Environment:
  MINDREADER_VERSION      Version or tag, for example 0.1.0 or v0.1.0; defaults to latest.
  MINDREADER_INSTALL_DIR  Install directory; defaults to $HOME/.local/bin.

Supported: x86_64/aarch64 GNU Linux and macOS. Windows users can use npm or the release .zip.
EOF
}

target() {
  os=$(uname -s); arch=$(uname -m)
  case "$os:$arch" in
    Linux:x86_64|Linux:amd64) printf '%s\n' x86_64-unknown-linux-gnu ;;
    Linux:aarch64|Linux:arm64) printf '%s\n' aarch64-unknown-linux-gnu ;;
    Darwin:x86_64|Darwin:amd64) printf '%s\n' x86_64-apple-darwin ;;
    Darwin:aarch64|Darwin:arm64) printf '%s\n' aarch64-apple-darwin ;;
    *) die "unsupported platform $os/$arch" ;;
  esac
}

release_tag() {
  if [ -n "${MINDREADER_VERSION:-}" ]; then
    case "$MINDREADER_VERSION" in v*) printf '%s\n' "$MINDREADER_VERSION" ;; *) printf 'v%s\n' "$MINDREADER_VERSION" ;; esac
    return
  fi
  response=$(curl -fsSL "https://api.github.com/repos/$repo/releases/latest") || die "failed to resolve latest release"
  latest=$(printf '%s\n' "$response" | sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$latest" ] || die "latest release did not include tag_name"
  printf '%s\n' "$latest"
}

verify_sha256() {
  archive_path=$1; checksum_path=$2
  if command -v sha256sum >/dev/null 2>&1; then
    (cd "$(dirname "$archive_path")" && sha256sum -c "$(basename "$checksum_path")") || die "checksum mismatch"
  elif command -v shasum >/dev/null 2>&1; then
    expected=$(awk '{print $1; exit}' "$checksum_path"); actual=$(shasum -a 256 "$archive_path" | awk '{print $1; exit}')
    [ "$actual" = "$expected" ] || die "checksum mismatch"
  else
    die "sha256sum or shasum is required"
  fi
}

main() {
  case "${1:-}" in -h|--help) usage; exit 0 ;; "") ;; *) usage >&2; die "unknown argument '$1'" ;; esac
  release_target=$(target); tag=$(release_tag)
  if [ -n "${MINDREADER_INSTALL_DIR:-}" ]; then install_dir=$MINDREADER_INSTALL_DIR; else [ -n "${HOME:-}" ] || die "HOME is not set"; install_dir=$HOME/.local/bin; fi
  archive="$bin-$tag-$release_target.tar.gz"
  url="https://github.com/$repo/releases/download/$tag/$archive"
  temp=$(mktemp -d "${TMPDIR:-/tmp}/mindreader-install.XXXXXX") || die "failed to create temporary directory"
  trap 'rm -rf "$temp"' 0 1 2 3 15
  curl -fsSL -o "$temp/$archive.sha256" "$url.sha256" || die "failed to download checksum"
  curl -fsSL -o "$temp/$archive" "$url" || die "failed to download archive"
  verify_sha256 "$temp/$archive" "$temp/$archive.sha256"
  tar -xzf "$temp/$archive" -C "$temp" || die "failed to extract archive"
  [ -f "$temp/$bin" ] || die "archive did not contain $bin"
  mkdir -p "$install_dir"; cp "$temp/$bin" "$install_dir/$bin"; chmod 755 "$install_dir/$bin"
  printf 'Installed %s to %s\n' "$bin" "$install_dir/$bin"
}

main "$@"

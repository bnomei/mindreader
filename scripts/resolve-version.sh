#!/usr/bin/env bash
set -euo pipefail

manifest_version="$({
  awk '
    /^\[package\]$/ { in_package = 1; next }
    /^\[/ { in_package = 0 }
    in_package && /^version[[:space:]]*=/ {
      value = $0
      sub(/^[^=]*=[[:space:]]*"/, "", value)
      sub(/"[[:space:]]*$/, "", value)
      print value
      exit
    }
  ' Cargo.toml
} || true)"

if [[ -z "$manifest_version" ]]; then
  echo "Could not resolve package version from Cargo.toml" >&2
  exit 1
fi

tag="${RELEASE_TAG:-${GITHUB_REF_NAME:-}}"
if [[ -n "$tag" ]]; then
  if [[ "$tag" != v* ]]; then
    echo "Release tag must start with v: $tag" >&2
    exit 1
  fi
  version="${tag#v}"
  if [[ "$version" != "$manifest_version" ]]; then
    echo "Release tag version $version does not match Cargo.toml version $manifest_version" >&2
    exit 1
  fi
else
  version="$manifest_version"
fi

mcp_version="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' mcp.json | head -n 1)"
if [[ "$mcp_version" != "$version" ]]; then
  echo "mcp.json version $mcp_version does not match package version $version" >&2
  exit 1
fi

npm_version="$(sed -n 's/.*"version"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' npm/mindreader/package.json | head -n 1)"
if [[ "$npm_version" != "$version" ]]; then
  echo "npm package version $npm_version does not match package version $version" >&2
  exit 1
fi

if ! grep -Fq "\"@bnomei/mindreader@$version\"" README.md; then
  echo "README.md does not pin the current package version $version in its MCP example" >&2
  exit 1
fi

if [[ -n "${GITHUB_OUTPUT:-}" ]]; then
  echo "version=$version" >> "$GITHUB_OUTPUT"
else
  echo "$version"
fi

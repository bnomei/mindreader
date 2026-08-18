#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW="$ROOT_DIR/.github/workflows/release.yml"
cd "$ROOT_DIR"

publishing_checkout_count="$(grep -Fc "ref: \${{ needs.release_ref.outputs.sha }}" "$WORKFLOW")"
post_crates_publish_count="$(grep -Fc "needs: [release_ref, github-release, crates-io]" "$WORKFLOW")"

if [[ "$publishing_checkout_count" != "6" ]]; then
  echo "Expected verify, build, GitHub Release, crates.io, container, and npm jobs to checkout the validated SHA; found $publishing_checkout_count" >&2
  exit 1
fi

if [[ "$post_crates_publish_count" != "2" ]]; then
  echo "Expected container and npm publishing to wait for crates.io; found $post_crates_publish_count" >&2
  exit 1
fi

for validation in 'git show-ref --verify --quiet' '^{commit}' 'git rev-parse HEAD' 'scripts/resolve-version.sh' 'stable SemVer prefixed with v' 'CARGO_REGISTRY_TOKEN' 'cargo publish --dry-run --locked -p mindreader' 'cargo publish --locked --no-verify -p mindreader'; do
  grep -Fq "$validation" "$WORKFLOW" || {
    echo "Release workflow is missing validation: $validation" >&2
    exit 1
  }
done

version="$(GITHUB_OUTPUT= GITHUB_REF_NAME=main scripts/resolve-version.sh)"
GITHUB_OUTPUT= RELEASE_TAG="v$version" scripts/resolve-version.sh >/dev/null
if GITHUB_OUTPUT= RELEASE_TAG=main scripts/resolve-version.sh >/dev/null 2>&1; then
  echo "Expected an explicit non-tag release ref to fail validation" >&2
  exit 1
fi

echo "Release workflow checkout and version contracts are valid."

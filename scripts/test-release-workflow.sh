#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORKFLOW="$ROOT_DIR/.github/workflows/release.yml"
publishing_checkout_count="$(grep -Fc "ref: \${{ needs.release_ref.outputs.sha }}" "$WORKFLOW")"

if [[ "$publishing_checkout_count" != "5" ]]; then
  echo "Expected verify, build, GitHub Release, container, and npm jobs to checkout the validated SHA; found $publishing_checkout_count" >&2
  exit 1
fi

for validation in 'git show-ref --verify --quiet' '^{commit}' 'git rev-parse HEAD' 'scripts/resolve-version.sh' 'stable SemVer prefixed with v'; do
  grep -Fq "$validation" "$WORKFLOW" || {
    echo "Release workflow is missing validation: $validation" >&2
    exit 1
  }
done

echo "Release workflow checkout and version contracts are valid."

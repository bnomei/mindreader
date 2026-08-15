# Packaging

GitHub Release assets are the source of truth for Mindreader's binary distributions. Releases
publish GNU Linux and macOS `.tar.gz` archives, a Windows `.zip`, optional static musl archives,
and a SHA-256 sidecar for every archive. Each archive contains the executable and MIT license.

The `@bnomei/mindreader` npm launcher and `scripts/install.sh` select the host target, download the
matching asset, verify its checksum, and cache or install the binary. The GHCR image is assembled
from the same GNU Linux release assets with `packaging/Dockerfile.release`.

To exercise the release pipeline locally for the current host target:

```bash
TARGET=aarch64-apple-darwin scripts/build-release.sh
VERSION=0.1.0 TARGET=aarch64-apple-darwin scripts/package-release.sh
VERSION=0.1.0 TARGET=aarch64-apple-darwin scripts/smoke-release.sh
```

Release tags, `Cargo.toml`, `mcp.json`, and the npm manifest must all use the same SemVer version.
The release workflow sets the published npm package version from that validated tag.

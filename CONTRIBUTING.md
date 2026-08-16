# Contributing

## Development

1. Copy `.env.example` to `.env` and set `NEO4J_PASSWORD`.
2. Start Neo4j (`docker compose up -d neo4j`) or use an existing instance.
3. Run:
   - `cargo fmt --all -- --check`
   - `cargo clippy --locked --all-targets --all-features -- -D warnings`
   - `cargo test --locked --all-targets --all-features`
   - `npm test --prefix npm/mindreader`
   - `cargo run --features developer-tools --bin mindreader-smoke` (when Neo4j is available)

## Graph model versioning

Mindreader supports only a fresh database for the current graph model. It records a model marker and rejects an unversioned non-empty database or a mismatched marker.

The current graph model is version 5. It stores dynamic layer membership arrays and shared feedback weights on nodes and relationships, stable relationship IRIs, expiring semantic activations backed by a vector index, and a synchronous whole-name merge-candidate index. There is no migration from earlier versions; recreate the Neo4j database or volume.

When introducing an incompatible model change, bump the model version, keep fresh bootstrap idempotent, document that operators must recreate the Neo4j database or volume, and do not add data backfills or compatibility migrations.

## Release discipline

- Treat user-visible behavior/tool-contract changes as SemVer-significant.
- Update `CHANGELOG.md` and keep `Cargo.toml`, `mcp.json`,
  `npm/mindreader/package.json`, and the pinned README MCP example on the same version.
- Push a `v<version>` tag only after CI passes. The release workflow validates the tagged commit,
  verifies the crates.io package, builds and smoke-tests every platform archive, then publishes
  GitHub, crates.io, GHCR, and npm releases.
- Configure the repository's `CARGO_REGISTRY_TOKEN` and `NPM_TOKEN` secrets before releasing.
  Missing registry credentials fail the release instead of silently skipping a required channel.

# Contributing

## Development

Install [Just](https://github.com/casey/just#installation) 1.58.0 or newer, then:

1. Copy `.env.example` to `.env` and set `NEO4J_PASSWORD`.
2. Start the isolated tools Neo4j (`docker compose --profile tools up -d neo4j-tools`) or use an existing disposable instance. Do not run smoke or bench against a live MCP database on port 7687.
3. Run:
   - `just check` for the fast compile loop
   - `just test` while developing
   - `just verify-full` before handoff; this runs formatting, Clippy with warnings denied, and all-target/all-feature tests
   - `npm test --prefix npm/mindreader`
   - `cargo run --features developer-tools --bin mindreader-smoke -- --config-dir packaging/tools-config` (when the tools Neo4j is available)

Build artifacts stay in the repository's normal `target/` directory. When Cargo's apparent-size summary exceeds 5 GiB, run `just target-report` and `just clean-preview`, review what Cargo would remove, and then choose a suitably scoped `cargo clean`. Both reporting recipes use Cargo's cross-platform dry-run mode; cleanup is intentionally manual rather than scheduled.

## Graph model versioning

Mindreader supports only a fresh database for the current graph model. It records a model marker and rejects an unversioned non-empty database or a mismatched marker.

The current graph model is version 9. It stores dynamic layer memberships, numeric shared judgment weights, per-fact Spike classifications, stable relationship IRIs, optional state effective intervals, exact revision history, expiring semantic activations backed by a vector index, and synchronous whole-name merge candidates. There is no migration from earlier versions; recreate the Neo4j database or volume.

When introducing an incompatible model change, bump the model version, keep fresh bootstrap idempotent, document that operators must recreate the Neo4j database or volume, and do not add data backfills or compatibility migrations.

## Release discipline

- Treat user-visible behavior/tool-contract changes as SemVer-significant.
- Update `CHANGELOG.md` and keep `Cargo.toml`, `mcp.json`,
  `npm/mindreader/package.json`, and the pinned README MCP example on the same version.
- Push a `v<version>` tag only after CI passes. The release workflow validates the tagged commit,
  verifies the crates.io package, builds and packages every platform archive, then publishes
  GitHub, crates.io, GHCR, and npm releases.
- Configure the repository's `CARGO_REGISTRY_TOKEN` and `NPM_TOKEN` secrets before releasing.
  Missing registry credentials fail the release instead of silently skipping a required channel.

# Contributing

## Development

1. Copy `.env.example` to `.env` and set `NEO4J_PASSWORD`.
2. Start Neo4j (`docker compose up -d neo4j`) or use an existing instance.
3. Run:
   - `cargo fmt -- --check`
   - `cargo test`
   - `cargo run --bin mindreader-smoke` (when Neo4j is available)

## Graph model versioning

Mindreader supports only a fresh database for the current graph model. Bootstrap records a model marker and rejects an unversioned non-empty database or a mismatched version.

For incompatible model changes, bump the graph model version, keep fresh bootstrap idempotent, document that operators must recreate the Neo4j database or volume, and do not add compatibility migrations or data backfills.

## Release discipline

- Treat user-visible behavior/tool-contract changes as SemVer-significant.
- Maintain changelog entries in release PR descriptions until a dedicated changelog file is introduced.


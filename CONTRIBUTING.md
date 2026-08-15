# Contributing

## Development

1. Copy `.env.example` to `.env` and set `NEO4J_PASSWORD`.
2. Start Neo4j (`docker compose up -d neo4j`) or use an existing instance.
3. Run:
   - `cargo fmt -- --check`
   - `cargo test`
   - `cargo run --bin mindreader-smoke` (when Neo4j is available)

## Schema migration/versioning strategy

mindreader bootstraps required constraints/indexes with `IF NOT EXISTS` and keeps schema evolution additive.

When introducing schema changes:
- keep migrations idempotent,
- prefer additive changes over destructive rewrites,
- include compatibility notes in PRs,
- bump crate version following SemVer.

## Release discipline

- Treat user-visible behavior/tool-contract changes as SemVer-significant.
- Maintain changelog entries in release PR descriptions until a dedicated changelog file is introduced.


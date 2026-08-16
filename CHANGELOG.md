# Changelog

All notable changes to Mindreader are documented here. The format follows Keep a Changelog, and
versions follow Semantic Versioning.

## [Unreleased]

### Changed

- Replaced process-bound projects with request-scoped, multi-membership layers on nodes and relationships.
- Added stable relationship IRIs and agent-directed node/relationship weights to retrieval ranking.
- Replaced opaque application errors with typed `thiserror` variants and retained source context.
- Updated every direct Rust dependency to its newest compatible release, including RMCP 3, reqwest 0.13, and TOML 1.1.
- Moved dynamic entity upserts to the least-privilege `apoc.merge.node` procedure and overlapped independent semantic lookup work.
- Moved complete search ranking and limiting into Neo4j, batched deterministic fact-lock acquisition, and replaced exhaustive merge-suggestion scans with an indexed candidate stage plus APOC reranking.
- Reduced semantic activation recall to metadata plus the selected convergence vector and bounded embedding-provider retries, latency, and response-body memory.

### Added

- `memory_feedback` for explicit strengthen/weaken signals and `memory_layers` for audited membership changes.
- `memory_merge` with advisory same-kind duplicate suggestions and explicit source/target direction.
- `memory_semantic_search` with provider embeddings and expiring, convergent Neo4j vector activations.

## [0.1.0] - 2026-08-15

### Added

- Deterministic Neo4j-backed MCP memory server with provenance and supersession.
- Cross-platform GitHub Release assets, npm launcher, shell installer, and GHCR image distribution.

[Unreleased]: https://github.com/bnomei/mindreader/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/bnomei/mindreader/releases/tag/v0.1.0

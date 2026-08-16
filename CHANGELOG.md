# Changelog

All notable changes to Mindreader are documented here. The format follows Keep a Changelog, and
versions follow Semantic Versioning.

## [Unreleased]

## [0.3.0] - 2026-08-16

### Changed

- Replaced the twelve-tool MCP surface with seven job-shaped tools: `memory_recall`, `memory_write`, `memory_revise`, `memory_withdraw`, `memory_judge`, `memory_place`, and `memory_unify`.
- Agent-facing dialect is `node` / `fact` / `literal`. Visibility is `scope`. Fact results carry a pasteable `target`.
- First use of a predicate creates a real global Property (`stub=false`). Class/Property catalog is `memory_recall` with `labels` Class or Property.
- Walk filters (`around` + `p`) match predicate name or IRI, not Neo4j relationship type.

### Removed

- MCP registration of `memory_get`, `memory_search`, `memory_semantic_search`, `memory_traverse`, `memory_stats`, `memory_schema`, `memory_assert`, `memory_replace`, `memory_retract`, `memory_feedback`, `memory_layers`, and `memory_merge`. In-process stats remain for smoke and bench.

## [0.2.0] - 2026-08-16

### Changed

- `memory_assert` now takes required `facts[]` (1–20 triples) and call-level `layers`. Scalar top-level `s`/`p`/`o` is gone.
- Recoverable tool failures return structured `isError` results (`{ok:false,reason,message}`) instead of JSON-RPC `-32602` for domain errors.
- `memory_schema` can list the Class/Property catalog with `list=true` (no Episode). Combining `list=true` with write fields is rejected before Cypher.
- Node and relationship JSON include `kind`. `memory_merge` returns `{node, episode}`.
- MCP advertise `2025-11-25`, negotiated versions `2024-11-05`…`2025-11-25` only, tools-only capabilities without `listChanged`.
- MCP handlers apply a 120/min burst-20 rate limit and a 45s invoke timeout after connect.

### Added

- Tool annotations, advertised runtime clamps, and host-compatible `outputSchema` objects on all twelve tools.

## [0.1.0] - 2026-08-15

### Added

- Deterministic Neo4j-backed MCP memory server with provenance and supersession.
- Cross-platform GitHub Release assets, npm launcher, shell installer, and GHCR image distribution.
- `memory_feedback` for explicit strengthen/weaken signals and `memory_layers` for audited membership changes.
- `memory_merge` with advisory same-kind duplicate suggestions and explicit source/target direction.
- `memory_semantic_search` with provider embeddings and expiring, convergent Neo4j vector activations.

### Changed

- Replaced process-bound projects with request-scoped, multi-membership layers on nodes and relationships.
- Added stable relationship IRIs and agent-directed node/relationship weights to retrieval ranking.
- Replaced opaque application errors with typed `thiserror` variants and retained source context.
- Updated every direct Rust dependency to its newest compatible release, including RMCP 3, reqwest 0.13, and TOML 1.1.
- Moved dynamic entity upserts to the least-privilege `apoc.merge.node` procedure and overlapped independent semantic lookup work.
- Moved complete search ranking and limiting into Neo4j, batched deterministic fact-lock acquisition, and replaced exhaustive merge-suggestion scans with an indexed candidate stage plus APOC reranking.
- Reduced semantic activation recall to metadata plus the selected convergence vector and bounded embedding-provider retries, latency, and response-body memory.
- Added crates.io and cargo-binstall distribution alongside the existing GitHub, npm, and container release channels.

[Unreleased]: https://github.com/bnomei/mindreader/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/bnomei/mindreader/releases/tag/v0.3.0
[0.2.0]: https://github.com/bnomei/mindreader/releases/tag/v0.2.0
[0.1.0]: https://github.com/bnomei/mindreader/releases/tag/v0.1.0

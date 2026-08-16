# Changelog

All notable changes to Mindreader are documented here. The format follows Keep a Changelog, and
versions follow Semantic Versioning.

## [0.5.0] - 2026-08-16

### Changed

- Require MCP `2026-07-28`, advertise only that protocol through `server/discover`, require self-contained request metadata, and reject older initialization protocols instead of negotiating a fallback.
- Advance the fresh-database graph contract to model v6 and use numeric weights only.
- Name-only `spike` writes mint `mindreader:element/<slug>` and keep the spike as an extra label instead of minting `mindreader:knowledge/<slug>`.
- Set-valued writes keep `review.alternatives` from sibling values and leave `facts[].conflicts` empty unless `contradicts` is true.
- Advisory unify suggestions require similarity ≥ 0.85 (Properties ≥ 0.92).
- `memory_recall.iris` applies `limit` per requested IRI. Concise iris recall keeps top-level `facts[]` empty and clears `paths`.
- MCP burst limit is 20 tokens (still 120/min and 45s). Semantic space mismatch returns `reason: embedding_space`.
- Smoke and bench use isolated Neo4j on port 7688 (`docker compose --profile tools` and `packaging/tools-config`) so they cannot rewrite a live MCP embedding space.
- Batched fuzzy merge-candidate lookup into two set-oriented Neo4j queries instead of two Bolt round trips per newly created entity.
- Fused embedding finiteness validation with squared-norm accumulation while preserving sequential floating-point order.
- Renamed the bundled `writing-to-mindreader` skill to `using-mindreader` and expanded it from write guidance into the complete recall, mutation, review, verification, feedback, placement, and unification flow.
- Sharpened the README and skill around the clerk loop: closed-world recall, set-valued writes versus revise, pasteable handles, and advisory review queues. Tightened MCP server instructions to prefer one recall rather than a full ritual.
- Standardized pasteable handles: `memory_unify` and `review.unify` use `{kind:"node", iri}` objects, every success result includes a `handles` bag, fact envelopes expose `current` / `rateable` / `mutable`, and text/semantic recall fill `nodes[]`.
- `memory_recall.iris` with `hops: 0` still returns incident fact handles on `lookups[i].facts`. Both recall tools accept `detail` (`concise` or `detailed`).
- Added `memory_recall` `history` for current and `validTo` facts of one node or fact IRI. Named-scope revise/withdraw of a global fact now says to retry with `scope: []`.

### Added

- Added Divan coverage for representative embedding dimensions from tiny through the supported maximum and expanded the live graph benchmark to 1/4/20 newly created merge candidates.

### Removed

- Removed the obsolete in-process memory API, legacy search implementation, permissive legacy target payloads, relationship-handle fallbacks, and alternate Neo4j URI attempts. This release has no compatibility aliases or database migration.

## [0.4.0] - 2026-08-16

### Changed

- Split closed-world `memory_recall` from the new provider-backed `memory_recall_semantic`; both default to 20 results and accept at most 100, while direct IRI recall accepts at most 20 node IRIs and preserves input order and misses.
- Made `memory_judge.ratings[]` and `memory_place.edits[]` atomic 1–20 item batches. Each successful changing batch records one public-tool Episode, and an invalid item rolls back the whole batch.
- Standardized successful results on `ok:true`, mutation no-op and Episode fields, and ordered batch summaries/items. Recoverable errors now state whether retry is safe and whether the mutation was definitely not applied.
- Replaced prescriptive follow-up fields with neutral `review.unify` and `review.alternatives`; corrections return the current `target` and retired `previousTarget`, while withdrawals return `withdrawnTargets`.
- Added concrete host-compatible schemas, top-level tool titles, explicit tool annotations, and field-level descriptions for all eight MCP tools.

### Removed

- Removed semantic mode from ordinary recall, scalar membership-edit payloads, legacy provenance names, and the remaining legacy result vocabulary. This release does not provide compatibility aliases.

## [0.3.0] - 2026-08-16

### Changed

- Replaced the twelve-tool MCP surface with seven job-shaped tools: `memory_recall`, `memory_write`, `memory_revise`, `memory_withdraw`, `memory_judge`, `memory_place`, and `memory_unify`.
- Agent-facing dialect is `node` / `fact` / `literal`. Visibility is `scope`. Fact results carry a pasteable `target`.
- First use of a predicate creates a real global Property (`stub=false`). Class/Property catalog is `memory_recall` with `labels` Class or Property.
- Walk filters (`around` + `p`) match predicate name or IRI, not Neo4j relationship type.

### Removed

- Removed the previous twelve-tool MCP surface. In-process statistics remain available to smoke and benchmark binaries.

## [0.2.0] - 2026-08-16

### Changed

- Assertions now take required `facts[]` (1–20 triples) and a call-level visibility array. Scalar top-level triples are gone.
- Recoverable tool failures return structured `isError` results (`{ok:false,reason,message}`) instead of JSON-RPC `-32602` for domain errors.
- The Class/Property catalog can be listed without creating an Episode. Combining catalog reads with write fields is rejected before Cypher.
- Node and relationship JSON include `kind`; unification returns the surviving node and Episode.
- MCP advertise `2025-11-25`, negotiated versions `2024-11-05`…`2025-11-25` only, tools-only capabilities without `listChanged`.
- MCP handlers apply a 120/min burst-20 rate limit and a 45s invoke timeout after connect.

### Added

- Tool annotations, advertised runtime clamps, and host-compatible `outputSchema` objects on the complete MCP surface.

## [0.1.0] - 2026-08-15

### Added

- Deterministic Neo4j-backed MCP memory server with provenance and supersession.
- Cross-platform GitHub Release assets, npm launcher, shell installer, and GHCR image distribution.
- Explicit strengthen/weaken signals, audited membership changes, and intentional same-kind unification.
- Provider embeddings with expiring, convergent Neo4j vector activations.

### Changed

- Replaced process-bound projects with request-scoped, multi-membership layers on nodes and relationships.
- Added stable relationship IRIs and agent-directed node/relationship weights to retrieval ranking.
- Replaced opaque application errors with typed `thiserror` variants and retained source context.
- Updated every direct Rust dependency to its newest compatible release, including RMCP 3, reqwest 0.13, and TOML 1.1.
- Moved dynamic entity upserts to the least-privilege `apoc.merge.node` procedure and overlapped independent semantic lookup work.
- Moved complete search ranking and limiting into Neo4j, batched deterministic fact-lock acquisition, and replaced exhaustive merge-suggestion scans with an indexed candidate stage plus APOC reranking.
- Reduced semantic activation recall to metadata plus the selected convergence vector and bounded embedding-provider retries, latency, and response-body memory.
- Added crates.io and cargo-binstall distribution alongside the existing GitHub, npm, and container release channels.

[Unreleased]: https://github.com/bnomei/mindreader/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/bnomei/mindreader/releases/tag/v0.5.0
[0.4.0]: https://github.com/bnomei/mindreader/releases/tag/v0.4.0
[0.3.0]: https://github.com/bnomei/mindreader/releases/tag/v0.3.0
[0.2.0]: https://github.com/bnomei/mindreader/releases/tag/v0.2.0
[0.1.0]: https://github.com/bnomei/mindreader/releases/tag/v0.1.0

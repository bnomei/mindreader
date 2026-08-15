# Changelog

All notable changes to Mindreader are documented here. The format follows Keep a Changelog, and
versions follow Semantic Versioning.

## [Unreleased]

### Changed

- Replaced process-bound projects with request-scoped, multi-membership layers on nodes and relationships.
- Added stable relationship IRIs and agent-directed node/relationship weights to retrieval ranking.

### Added

- `memory_feedback` for explicit strengthen/weaken signals and `memory_layers` for audited membership changes.

## [0.1.0] - 2026-08-15

### Added

- Deterministic Neo4j-backed MCP memory server with provenance and supersession.
- Cross-platform GitHub Release assets, npm launcher, shell installer, and GHCR image distribution.

[Unreleased]: https://github.com/bnomei/mindreader/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/bnomei/mindreader/releases/tag/v0.1.0

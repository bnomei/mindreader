# AGENTS.md

## Scope

These instructions apply to the entire repository.

## Project contract

Mindreader is a Rust stdio MCP server backed by Neo4j. It exposes exactly ten memory tools and stores explicit graph triples with provenance, request-scoped multi-layer visibility, shared explicit feedback weights, auditable layer memberships, soft retraction, and explicit supersession history. Treat the behavior in `src/` as authoritative; keep `README.md`, `mcp.json`, and `skills/writing-to-mindreader/SKILL.md` aligned with it.

Aim for feature-complete changes. Do not ship MVP-only slices, preserve backward compatibility, or add abstraction for hypothetical future requirements. Choose the simplest implementation that fully satisfies the current contract.

## Repository map

- `src/main.rs`: process startup and stdio transport. Stdout is protocol-only.
- `src/server.rs`: MCP tool registration, advertised JSON schemas, protocol negotiation, and lazy Neo4j access.
- `src/service.rs`: typed application boundary shared by MCP and other adapters.
- `src/domain.rs`: validated layer, entity, literal, target, replacement, and retraction concepts.
- `src/tools.rs`: public tool arguments and graph behavior.
- `src/graph.rs`: Neo4j connection/bootstrap, query helpers, serialization, safe labels/relationships, and persistence primitives.
- `src/config.rs`: `.env` loading and environment defaults.
- `src/layers.rs`: layer validation and visibility-union policy.
- `src/iri.rs`: deterministic IRI minting and kind/label mapping.
- `src/bin/mindreader-smoke.rs`: live Neo4j integration coverage.
- `scripts/mcp_handshake_probe.py`: portable stdio handshake diagnostic.
- `docker-compose.yml` and `Dockerfile`: local Neo4j and containerized server paths.

## Non-negotiable invariants

- In MCP serving mode, never print logs, banners, or diagnostics to stdout. MCP JSON-RPC owns stdout; write diagnostics to stderr. The explicit `--help` and `--version` CLI exits may print their requested output there.
- Keep MCP initialization and `tools/list` independent of Neo4j availability. Database connection and bootstrap remain lazy for the stdio server.
- Keep `NEO4J_PASSWORD` required and secrets environment-only. Never log credentials.
- Parameterize user-provided Cypher values. Validate any identifier that must be interpolated, such as labels or relationship types.
- Every scoped tool requires a validated `layers` array. `[]` means global-only; named layers form an OR union, and visible relationships require visible endpoints. Layer IDs use lowercase kebab-case colon namespaces.
- Empty record memberships mean global. An exact relationship has one identity across memberships; assertions merge memberships rather than duplicate the relationship.
- `memory_schema` writes global schema-as-data and is the only tool without a `layers` input.
- `memory_feedback` changes a visible node or current relationship's shared signed weight by exactly `+1` or `-1`. Retrieval never changes weight automatically, weights do not decay, and search uses weight only within the same Spike category.
- `memory_layers` records state-changing membership audits and must preserve relationship endpoint closure.
- Retraction is soft: set `validTo`; do not hard-delete nodes or history.
- Ordinary assertions are set-valued. Reasserting the exact `(subject, property, object)` merges memberships or is a no-op; asserting another object preserves every current value.
- Corrections are explicit: `memory_replace` moves only the requested memberships off the selected old fact, preserves unrelated current values and memberships, and creates `SUPERSEDES` history in the same transaction.
- `CONTRADICTS` and `SUPERSEDES` are system-owned. Client commands must not assert, replace, or retract them directly.
- Keep `CONTRADICTS` multi-valued and idempotent per exact pair.
- Every state-changing mutation records exactly one `Episode` and associates provenance with the changed records. No-op mutations record none.
- Preserve the MCP host compatibility rule in `src/server.rs`: advertised input schemas remain plain tagged object schemas and contain no `anyOf` or `oneOf`.
- Keep the registered tool list synchronized across `src/server.rs`, its tests, `mcp.json`, and the README.

## Working conventions

- Search with `rg` or `rg --files` before editing.
- Use `apply_patch` for hand-authored file changes.
- Use UV for Python: run the handshake probe as `uv run scripts/mcp_handshake_probe.py`. Do not invoke Python directly.
- Reuse the repository's normal `target/` directory. Do not set `CARGO_TARGET_DIR` to `/private/tmp`, `.codex-cache`, or another secondary location. If Cargo holds its lock, wait and poll; use `cargo clean` for disk pressure.
- Preserve unrelated work in a dirty tree. Do not reset or overwrite user changes.
- Update source-facing documentation when tool inputs, defaults, limits, environment variables, layering, IRI rules, or operational behavior changes.
- The graph model is fresh-database-only. Bootstrap is idempotent for the current model marker, but incompatible or unversioned non-empty databases must fail with reset/recreate guidance; do not add data migrations.

## Validation

Run the checks appropriate to the change and fix every actionable failure:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

For changes that affect graph queries, persistence semantics, layers, configuration, or tool behavior, also start Neo4j and run the live smoke suite:

```bash
docker compose up -d neo4j
cargo run --bin mindreader-smoke
```

The smoke test mutates the configured database and does not clean up its fixtures. Use a development or disposable database, never an unreviewed production target.

For MCP registration, input-schema, startup, or protocol changes, verify that initialization and `tools/list` complete with all ten tools even when Neo4j is unavailable. Use the existing unit tests and, when the workspace paths match the script, the probe:

```bash
cargo build
uv run scripts/mcp_handshake_probe.py
```

For documentation-only changes, at minimum verify every command, path, environment variable, default, limit, and tool name against source, then run formatting checks that do not require Neo4j.

## Change checklist

Before handing off a change:

1. Confirm layer filters and endpoint closure apply to every scoped read and mutation.
2. Confirm supersession and replacement remain in one Neo4j transaction.
3. Confirm new dynamic Cypher identifiers pass through an allowlist validator.
4. Confirm stdout stays protocol-clean.
5. Confirm MCP tool names and schemas pass unit tests and remain host-compatible.
6. Update `README.md`, `mcp.json`, and the writing skill when their documented contract changes.
7. Report which checks ran and whether the live Neo4j smoke test was skipped.

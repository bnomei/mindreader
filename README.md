# mindreader

[![CI](https://github.com/bnomei/mindreader/actions/workflows/ci.yml/badge.svg)](https://github.com/bnomei/mindreader/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/@bnomei/mindreader)](https://www.npmjs.com/package/@bnomei/mindreader)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Mindreader is a deterministic [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) memory server backed by Neo4j. It stores explicit graph triples (`subject -predicate-> object`) and serves twelve tools over stdio for scoped retrieval, semantic recall, traversal, feedback, layer auditing, entity merging, schema management, and durable writes.

Mindreader does not extract facts with a hidden language model or expose raw Cypher. Ordinary memory writes preserve history and record an `Episode`; explicit corrections close the selected old fact and link its replacement with `SUPERSEDES`. Semantic search is optional and sends its query text to the selected OpenAI or xAI embedding API.

## What mindreader provides

- A shared, inspectable memory graph for local agents and multi-agent teams.
- Deterministic IRI minting for entities, schema, literals, and provenance episodes.
- Request-scoped visibility across one or many named layers, with global memory available in every scope.
- Wakeup-oriented retrieval that returns current facts and ranked `Signal`, `Pattern`, `Insight`, and `Knowledge` context.
- Semantic recall that blends direct results with nearby, expiring result bundles stored in Neo4j.
- Explicit strengthen/weaken feedback, shared signed weights, and auditable membership edits.
- Idempotent assertions, advisory duplicate suggestions, explicit node merging, transactional supersession, contradiction links, and soft retraction.
- A stdio MCP server that completes the MCP handshake before Neo4j is ready and connects to the database lazily.

## Install a release

The npm launcher supports x64 and arm64 GNU/glibc Linux and macOS, plus x64 Windows. On first use it
downloads the matching GitHub Release binary, verifies its SHA-256 checksum, and stores it in a
versioned cache:

```bash
npx -y @bnomei/mindreader@0.1.0 --version
```

GNU Linux and macOS users can install the latest release directly:

```bash
curl -fsSL https://raw.githubusercontent.com/bnomei/mindreader/main/scripts/install.sh | sh
```

Checksummed archives are available from [GitHub Releases](https://github.com/bnomei/mindreader/releases),
and the release container is published to GHCR:

```bash
docker run --rm ghcr.io/bnomei/mindreader:0.1.0 --version
```

The npm launcher honors `MINDREADER_VERSION`, `MINDREADER_REPOSITORY`, and
`MINDREADER_NPM_CACHE` for version, mirror, and cache overrides.
Static musl archives for x64 and arm64 Linux are available as manual release downloads.

## Quickstart from source

This path runs Neo4j in Docker, verifies the graph behavior, and builds a local MCP server binary.

### Prerequisites

- Docker with Docker Compose. Neo4j must provide matching APOC Core and APOC Extended plugins; the included Compose service installs both.
- Rust 1.89 or newer with Cargo

### 1. Configure the server

```bash
cp .env.example .env
```

Set a private `NEO4J_PASSWORD` in `.env`. The example password is intended only for local development.
This repository-level file supplies Docker Compose. A native Mindreader process instead uses its OS configuration directory, described in [Configuration reference](#configuration-reference).

### 2. Start Neo4j

```bash
docker compose up -d neo4j
```

Neo4j listens on `bolt://127.0.0.1:7687`; its browser UI is available at `http://127.0.0.1:7474`.
Mindreader does not migrate older graph models. For an existing local development volume, recreate it with `docker compose down --volumes` before starting Neo4j.

### 3. Verify the memory behavior

```bash
NEO4J_PASSWORD='<same-password-as-repository-.env>' cargo run --bin mindreader-smoke
```

The smoke process reads secrets from the native Mindreader `.env`, or from its process environment as shown above.

The smoke test creates test graph data and exercises schema creation, dynamic multi-layer visibility, membership merging, scoped replacement and retraction, signed feedback ranking, concurrent feedback, membership auditing, endpoint closure, and stable relationship retrieval. A successful run ends with:

```text
ALL PASS
```

Important: the smoke binary persists its fixtures and does not clean them up. Run it only against a development or disposable Neo4j database.

### 4. Build the MCP server

```bash
cargo build --release --bin mindreader
```

The executable is `target/release/mindreader`. Add it to an MCP client using the [client configuration](#connect-an-mcp-client) below.

Run the binary without arguments to serve MCP. It also accepts `-h`/`--help` and `-V`/`--version`; other arguments are rejected.

## Connect an MCP client

Mindreader uses stdio transport. The MCP client starts the process and exchanges JSON-RPC messages over its standard input and output.

### Local binary

Use absolute paths because desktop MCP clients do not necessarily inherit your shell's working directory or `PATH`.

```json
{
  "mcpServers": {
    "mindreader": {
      "command": "/absolute/path/to/mindreader/target/release/mindreader",
      "env": {
        "NEO4J_PASSWORD": "<NEO4J_PASSWORD>",
        "OPENAI_API_KEY": "<OPTIONAL_OPENAI_API_KEY>"
      }
    }
  }
}
```

Configure the Neo4j URI, username, provider models, and semantic-search policy in `config.toml`, not in MCP environment variables. Restart the MCP client after changing its configuration. A successful connection exposes the twelve tools listed in [MCP tool reference](#mcp-tool-reference).

### npm launcher

Pin the package version in MCP configuration so the client always starts the same release:

```json
{
  "mcpServers": {
    "mindreader": {
      "command": "npx",
      "args": ["-y", "@bnomei/mindreader@0.1.0"],
      "env": {
        "NEO4J_PASSWORD": "<NEO4J_PASSWORD>",
        "XAI_API_KEY": "<OPTIONAL_XAI_API_KEY>"
      }
    }
  }
}
```

### Docker Compose

The Compose service builds and runs Mindreader while Neo4j uses the persistent `neo4j-data` volume.

```json
{
  "mcpServers": {
    "mindreader": {
      "command": "docker",
      "args": [
        "compose",
        "-f",
        "/absolute/path/to/mindreader/docker-compose.yml",
        "run",
        "--rm",
        "-T",
        "mindreader"
      ],
      "cwd": "/absolute/path/to/mindreader"
    }
  }
}
```

Run `docker compose up -d neo4j` before the client launches the Compose-backed server. Compose reads the repository's `.env` file.

## Recommended agent workflow

Start each session by recovering relevant context, then write only durable facts:

1. Choose the request's `layers` visibility union, then call `memory_search` for exact names or `memory_semantic_search` for conceptual recall when you do not have an IRI.
2. Call `memory_get` for a returned IRI when you need the exact node or its immediate neighbors.
3. Call `memory_traverse` when you need typed paths beyond one hop.
4. Call `memory_schema` only when the required Class or Property does not exist.
5. Call `memory_assert` once per durable, future-relevant triple.
6. Review any `mergeSuggestions`. They are fuzzy, advisory candidates rather than proof that two nodes are identical. Call `memory_merge` only after deciding the identities truly match.
7. Search again to verify an important write.
8. After using a retrieved node or relationship, call `memory_feedback` with `strengthen` if it helped or `weaken` if it did not.

Do not store task chatter, acknowledgements, markdown dumps, or transient status as memory. The repository includes a reusable decision guide at [`skills/writing-to-mindreader/SKILL.md`](skills/writing-to-mindreader/SKILL.md).

## Minimal assert, search, and traverse example

Call `memory_assert`:

```json
{
  "s": { "kind": "entity", "name": "Alice" },
  "p": "worksOn",
  "o": { "kind": "entity", "name": "mindreader" },
  "layers": ["project:graph-memory"]
}
```

The response includes subject, object, and stable relationship IRIs. Use the returned subject IRI in later calls:

```json
{
  "text": "Alice",
  "layers": ["project:graph-memory", "project:agent-runtime"]
}
```

```json
{
  "from": "mindreader:element/alice",
  "depth": 2,
  "layers": ["project:graph-memory"]
}
```

Reasserting the same current triple returns `"noop": true`. Asserting a different object adds another current value. Use `memory_replace` when one exact current value is a correction of another.

After a returned relationship helps, strengthen its shared weight explicitly:

```json
{
  "layers": ["project:graph-memory"],
  "target": {
    "kind": "relationship",
    "iri": "mindreader:relationship/<returned-uuid>"
  },
  "mode": "strengthen"
}
```

To audit its memberships independently of the fact value, use `memory_layers` with the same stable target and at least one `add` or `remove` entry.

## MCP tool reference

All tools return structured JSON. Every scoped tool requires a `layers` array; the global `memory_schema` and permanent `memory_merge` operations do not. In the table, `@layers` is short for that request field.

| Tool | Required input | Optional input and defaults | Purpose |
| --- | --- | --- | --- |
| `memory_search` | `layers` | `text`, `labels`, `limit` (`20`, clamped to `1..100`) | Find current visible facts and ranked `ABOUT` context. The result limit is applied only after Spike, weight, relevance, and deterministic tie-break ranking. |
| `memory_semantic_search` | non-empty `text`, `layers` | `labels`, `limit` (`20`, clamped to `1..100`) | Embed the query, blend direct matches with nearby remembered result bundles, and return current visible facts with a 1-based `rank`. |
| `memory_get` | `iri`, `layers` | `hops` (`0`; only `1` includes neighbors) | Fetch a visible node and, optionally, its current visible one-hop relationships. |
| `memory_traverse` | `from`, `layers` | `rels` (all fixed relationships), `depth` (`1`, clamped to `1..3`), `limit` (`50`, clamped to `1..200`) | Walk current visible typed relationships in either direction. |
| `memory_stats` | `layers` | None | Report graph-model readiness, visible node/edge counts, database-wide episode count, and per-membership active edge totals. |
| `memory_assert` | tagged `s`, `p`, tagged `o`, `layers` | `spike`, `contradicts` (`false`) | Add one set-valued triple or merge memberships into its existing relationship identity. |
| `memory_replace` | tagged `s`, `p`, tagged `old`, tagged `new`, `layers` | `spike`, `contradicts` (`false`), `reason` | Move the selected memberships from one exact value to its correction atomically. |
| `memory_retract` | tagged `target`, `layers` | `reason` | Remove selected memberships and soft-close a fact when its last membership is removed. |
| `memory_feedback` | `layers`, tagged `target`, `mode` | None | Apply exactly `+1` (`strengthen`) or `-1` (`weaken`) to a visible node or current relationship's shared weight. |
| `memory_layers` | `layers`, tagged `target` | `add`, `remove` (at least one entry across them) | Audit and atomically edit one node or current relationship's memberships. |
| `memory_schema` | `kind`, plus `name` or `iri` | `subClassOf`, `subPropertyOf`, `domain`, `range` | Declare a Class or Property and its structural links as global records. |
| `memory_merge` | same-kind `source`, `target` IRIs | None | Permanently merge two user-visible non-literal entities across every membership and historical relationship; the target IRI and name survive. Property merges also rewrite facts to the surviving predicate and consolidate exact duplicates. |

`memory_assert`, `memory_replace`, and `memory_schema` return `mergeSuggestions` when they create a user-visible entity with a fuzzy same-kind name match. Each suggestion includes the two names, its similarity, and a directly callable `merge: {source, target}` payload. The shorter name is recommended as the target; ties keep the pre-existing candidate. This direction is only a recommendation: inspect identity carefully and reverse or ignore it when appropriate. Names such as `007` and `007s` can be similar while still naming different entities.

### Assertion values

Subjects and entity objects use an explicit `kind` tag and require `iri` or `name`:

```json
{ "kind": "entity", "iri": "mindreader:element/alice", "labels": ["Element"] }
```

Literal objects use the `literal` tag:

```json
{
  "kind": "literal",
  "value": "2026-08-15",
  "datatype": "xsd:date"
}
```

All mutation inputs are tagged objects at both the MCP and runtime boundaries. Their advertised schemas deliberately avoid union keywords such as `anyOf` and `oneOf` for compatibility with strict MCP hosts.

An exact replacement identifies both values explicitly:

```json
{
  "s": { "kind": "entity", "name": "Alice" },
  "p": "worksOn",
  "old": { "kind": "entity", "name": "old-project" },
  "new": { "kind": "entity", "name": "new-project" },
  "layers": ["project:graph-memory"],
  "reason": "corrected assignment"
}
```

Retraction uses `target.kind`: `fact` requires `s`, `p`, and `o`; `predicate` requires `s` and `p`; `subject` accepts only `s`. Predicate and subject scopes are intentionally broad.

`spike` must be one of `Signal`, `Pattern`, `Insight`, or `Knowledge`. When a spike subject points to an `Element`, Mindreader also maintains an `ABOUT` relationship. Search ranks categories from `Knowledge` down to `Signal`, then uses the sum of subject, relationship, and object weights within the same Spike category, then text relevance.

### Fixed relationship types

Mindreader traverses the following relationship types by default:

```text
INSTANCE_OF, SUBCLASS_OF, SUBPROPERTY_OF, DOMAIN, RANGE,
ASSERTS, ABOUT, EVIDENCE_FOR, DERIVED_FROM, SUPPORTS,
CONTRADICTS, SUPERSEDES
```

Properties outside the structural set are stored as `ASSERTS` relationships with a `propertyIri`.

## Memory model

### Layers

Layer scope is dynamic per request. The required `layers` array is an OR union: a named record is visible when any membership intersects the requested names. Empty memberships mean global, so global records are visible in every request. An empty request selects only global records:

```json
{ "layers": [] }
```

Layer IDs match lowercase kebab-case segments separated by colons, such as `project:graph-memory` or `analysis:hypothesis`. Colons provide naming namespaces, not hierarchy. `@layers` is the short form used in tool descriptions for the JSON `layers` field.

Nodes and relationships both carry memberships. A relationship is returned only when the relationship and both endpoints are visible in the request scope; traversal applies that closure to every path. Assertions inherit memberships onto endpoints. One exact `(subject, property, object)` has one current relationship identity across all memberships: reasserting it with another named layer merges that membership rather than creating a duplicate relationship. An existing global relationship stays global.

For named memberships, `memory_replace` and `memory_retract` affect only the requested memberships; the old relationship becomes historical only after its last membership is removed. With `layers: []`, they operate on global records. `memory_layers` explicitly audits one visible node or current relationship, records a state-changing edit as an `Episode`, never propagates the edit, and rejects membership states that would expose a relationship without both endpoints.

Layers are visibility filters, not strict tenant isolation. Anyone with MCP access can request any valid layer name. Use separate Neo4j databases when you need a hard data-isolation boundary.

### Feedback and ranking

Retrieval returns stable IRIs for nodes and relationships. Pass one back to `memory_feedback` as `target: {"kind":"node|relationship","iri":"..."}` together with the retrieval scope. `strengthen` always adds exactly `1`; `weaken` subtracts exactly `1`. Weight is a signed integer shared by the record across all memberships.

Retrieval never changes weight automatically, feedback can arrive in a later turn, and there is no time decay. Search first orders facts by Spike category (`Knowledge > Insight > Pattern > Signal`), then by the combined subject + relationship + object weight within a category, then by text relevance. A relationship feedback target must still be current, and every target must remain visible in `@layers`.

### Semantic recall

`memory_semantic_search` embeds the query with one external provider, runs the ordinary direct search, retrieves nearby semantic activations from Neo4j's cosine vector index, resolves their relationship IRIs against the current graph and requested scope, and combines the rankings with weighted reciprocal-rank fusion. Direct matches have weight `2.0` by default; recalled bundles are weighted by vector similarity. Recalled facts must still be current, match `labels`, and satisfy relationship-and-endpoint visibility in `@layers`.

An activation is an internal `SemanticActivation:TTL` node containing only an embedding vector, a ranked `resultRefs` list of stable relationship IRIs, and an expiry timestamp. Similar searches with sufficiently overlapping results converge into one activation and refresh its TTL; other searches create a new activation. The default TTL is 30 days. Expired activations are excluded immediately and APOC Extended removes them in the background. Activation maintenance does not create an `Episode` or change feedback weights.

The embedding provider, model, and dimensions define one vector space for the database. If that selection changes, Mindreader discards only the ephemeral activations and recreates their vector index; durable graph memory remains intact. Semantic search requires an embedding key, but all other tools remain usable without one.

### Provenance and history

Each state-changing memory mutation creates one `Episode` with the tool name and timestamp; no-op mutations create none. Internal semantic-activation maintenance is excluded. Relationships carry the episode identifier. Retraction sets `validTo` and optionally records a reason; it does not delete graph nodes. `CONTRADICTS` and `SUPERSEDES` are system-owned history predicates and cannot be asserted, replaced, or retracted directly.

Ordinary assertions are set-valued: the same `(subject, property, object)` relationship is idempotent apart from membership merges, while a different object becomes another current value. `memory_replace` is the correction operation: it moves only the selected memberships from the requested old fact, preserves unrelated current values and memberships, and creates `SUPERSEDES` history in one Neo4j transaction.

Mutations acquire deterministic graph locks and retry Neo4j transient transaction failures with bounded backoff. Commit failures are never retried because their outcome may be ambiguous.

`CONTRADICTS` is multi-valued. Setting `contradicts: true` on an assertion records explicit links to conflicting visible current objects.

`memory_merge` is the intentional destructive exception to soft history: it requires matching canonical kinds and removes the source node after moving its memberships and current and historical relationships onto the target. Bootstrap-seeded Class and Property IRIs are permanent targets and cannot be sources. It records exactly one merge `Episode`, preserves the target IRI and name, combines memberships and weights, marks moved relationships with merge provenance, and soft-closes merge-created self-relations and duplicate facts. Merging Properties rewrites their predicate references and refreshed search text transactionally, but both Properties must use the same structural relationship representation; system-owned `CONTRADICTS` and `SUPERSEDES` Properties cannot be merged. It creates no alias. Review the direction before calling it.

### Deterministic IRIs

Mindreader accepts explicit IRIs or derives them from names:

| Kind | Pattern | Slug case |
| --- | --- | --- |
| Class | `mindreader:class/<slug>` | Preserved |
| Property | `mindreader:property/<slug>` | Preserved |
| Element | `mindreader:element/<slug>` | Lowercase |
| Literal | `mindreader:literal/<slug>-<hash>` | Lowercase |
| Episode | `mindreader:episode/<uuid>` | Lowercase |
| Signal, Pattern, Insight, Knowledge | `mindreader:<kind>/<slug>` | Lowercase |

Slug normalization keeps ASCII letters, numbers, `.`, `_`, and `-`; replaces other runs with `-`; trims surrounding dashes; and falls back to `unnamed`. Literal identity includes its datatype and value.

## Security and trust boundary

Mindreader does not add an application authentication layer in front of MCP or Neo4j, and layers are a relationship filter rather than a security boundary. Anyone who can start or control the MCP process can use its configured Neo4j credential and embedding provider key. Ordinary graph operations remain local, but every `memory_semantic_search` sends its query text to OpenAI or xAI for embedding. It does not send the stored result bundle. Secure the client configuration, native `.env`, host process, network path, Neo4j deployment, and provider accounts accordingly.

## Configuration reference

On first MCP startup, Mindreader creates `config.toml` and `.env` without overwriting existing files in the OS-native configuration directory:

- Linux: `${XDG_CONFIG_HOME:-$HOME/.config}/mindreader`
- macOS: `~/Library/Application Support/mindreader`
- Windows: `%APPDATA%\mindreader`

The directory is user-only on Unix and the generated `.env` is mode `0600`. Put non-secret settings in `config.toml`:

```toml
[neo4j]
uri = "bolt://127.0.0.1:7687"
user = "neo4j"

[embeddings.openai]
model = "text-embedding-3-small"
dimensions = 1536

[embeddings.xai]
model = ""
dimensions = 1536

[semantic]
ttl_days = 30
neighbor_limit = 10
recall_similarity_threshold = 0.70
convergence_similarity_threshold = 0.90
convergence_result_overlap_threshold = 0.60
rrf_k = 60.0
direct_weight = 2.0
```

The TOML parser rejects unknown fields. Dimensions must be `1..4096`; semantic similarity thresholds must be finite values from `0` through `1`; TTL, neighbor limit (`1..100`), `rrf_k`, and `direct_weight` must be positive.

The colocated `.env` contains only secrets:

```dotenv
NEO4J_PASSWORD=
OPENAI_API_KEY=
XAI_API_KEY=
```

Non-empty process environment secrets take precedence over that file. `NEO4J_PASSWORD` is required for database-backed calls. If `OPENAI_API_KEY` is set, Mindreader uses the configured OpenAI model. Otherwise, if `XAI_API_KEY` is set, it uses the configured xAI model. OpenAI therefore wins when both keys exist. The selected provider's model must be non-empty and its configured dimension must match the returned vectors. Non-secret `NEO4J_URI`, `NEO4J_USER`, model, dimension, and semantic settings are not read from environment variables.

At first database use, Mindreader requires matching APOC Core and APOC Extended installations, verifies the text-similarity, node-merge, and TTL functions and procedures, then marks model version 4, creates required uniqueness, full-text, and optional vector indexes, seeds base schema entities, and waits for indexes. APOC TTL must be enabled. The included Docker Compose configuration installs both plugins and grants only the required APOC surface.

Version 4 requires a fresh database. There is no migration path: an unversioned non-empty or incompatible database is rejected with instructions to recreate the Neo4j database or volume.

## Run entirely with Docker

Build the image:

```bash
docker build -t mindreader:local .
```

Start Neo4j and run the stdio server:

```bash
docker compose up -d neo4j
docker compose run --rm -T mindreader
```

The second command waits for MCP input on stdin without allocating a pseudo-TTY. All diagnostics go to stderr so stdout remains reserved for MCP messages; startup messages use structured JSON, while some database bootstrap warnings are plain text.

## Development

Format, lint, and run the unit tests:

```bash
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Run the live integration smoke test when Neo4j is available:

```bash
cargo run --bin mindreader-smoke
```

The smoke test writes persistent fixtures to the configured Neo4j database and does not clean them up. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for graph model versioning and release expectations.

## Repository map

| Path | Responsibility |
| --- | --- |
| [`src/server.rs`](src/server.rs) | MCP tool registration, advertised input schemas, protocol negotiation, and lazy database access. |
| [`src/service.rs`](src/service.rs) | Typed application boundary used by transport adapters. |
| [`src/domain.rs`](src/domain.rs) | Validated layer IDs and tagged entity, literal, target, replacement, and retraction concepts. |
| [`src/tools.rs`](src/tools.rs) | Tool arguments, scoped graph behavior, advisory merge suggestions, feedback, membership auditing, supersession, contradiction, and retraction. |
| [`src/merge.rs`](src/merge.rs) | Permanent entity merging and APOC-backed fuzzy suggestions. |
| [`src/semantic.rs`](src/semantic.rs) | Direct-plus-vector semantic ranking, activation convergence, and TTL refresh. |
| [`src/embeddings.rs`](src/embeddings.rs) | Native OpenAI and xAI embedding clients and vector validation. |
| [`src/graph.rs`](src/graph.rs) | Neo4j connection/bootstrap, graph serialization, safe identifiers, vector-index setup, and provenance helpers. |
| [`src/config.rs`](src/config.rs) | Native TOML and secret-file initialization, validation, provider selection, and defaults. |
| [`src/layers.rs`](src/layers.rs) | Layer validation and visibility-union policy. |
| [`src/iri.rs`](src/iri.rs) | IRI detection, slugging, minting, and kind/label mappings. |
| [`src/bin/mindreader-smoke.rs`](src/bin/mindreader-smoke.rs) | Live end-to-end graph behavior checks. |
| [`scripts/mcp_handshake_probe.py`](scripts/mcp_handshake_probe.py) | Portable MCP initialize and tool-discovery diagnostic. |
| [`mcp.json`](mcp.json) | Server metadata, transport, environment, and exported tool inventory. |

## Troubleshooting

### `NEO4J_PASSWORD is not set`

Set `NEO4J_PASSWORD` in the `.env` beside the generated `config.toml`, or in the MCP process environment, then restart the server process or client. The repository `.env.example` is for Docker Compose rather than native config discovery.

### Semantic search reports that no provider is configured

Set `OPENAI_API_KEY` or `XAI_API_KEY` in the native `.env` or process environment. If using xAI, also set a non-empty `[embeddings.xai].model` and the matching dimensions in `config.toml`. Provider failure does not fall through to the other provider because their vector spaces are not interchangeable.

### A database call reports a missing APOC function or procedure

Install APOC Core and APOC Extended versions matching Neo4j, enable APOC TTL, and allow the text, merge, and TTL calls shown in `docker-compose.yml`. Recreate a development database after changing an incompatible graph model.

### Neo4j connection failures

Check the container and credentials:

```bash
docker compose ps
docker compose logs neo4j
```

Mindreader tries the configured endpoint up to three times with bounded backoff. MCP initialization and tool discovery can still succeed before Neo4j connects; the first database-backed tool call reports the connection error if Neo4j remains unavailable.

### The MCP client connects but lists no tools

Confirm that the configured command uses an absolute executable path, the process can read its environment, and stdout is not redirected through a wrapper that prints status messages. Mindreader writes its own diagnostics to stderr.

### A request reports an invalid layer

Use lowercase kebab-case segments separated by colons, for example `project:graph-memory`. Pass `layers: []` when only global records should participate.

## License

Mindreader is available under the [MIT License](LICENSE).

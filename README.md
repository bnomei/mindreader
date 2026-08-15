# mindreader

[![CI](https://github.com/bnomei/mindreader/actions/workflows/ci.yml/badge.svg)](https://github.com/bnomei/mindreader/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/@bnomei/mindreader)](https://www.npmjs.com/package/@bnomei/mindreader)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

Mindreader is a deterministic, privacy-first [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) memory server backed by Neo4j. It stores explicit graph triples (`subject -predicate-> object`) and serves ten tools over stdio for scoped retrieval, traversal, feedback, layer auditing, schema management, and durable writes.

Mindreader does not extract facts with a hidden language model, require embeddings, expose raw Cypher, or destructively overwrite history. Every state-changing write records an `Episode`; explicit corrections close the selected old fact and link its replacement with `SUPERSEDES`.

## What mindreader provides

- A shared, inspectable memory graph for local agents and multi-agent teams.
- Deterministic IRI minting for entities, schema, literals, and provenance episodes.
- Request-scoped visibility across one or many named layers, with global memory available in every scope.
- Wakeup-oriented retrieval that returns current facts and ranked `Signal`, `Pattern`, `Insight`, and `Knowledge` context.
- Explicit strengthen/weaken feedback, shared signed weights, and auditable membership edits.
- Idempotent assertions, transactional supersession, explicit contradiction links, and soft retraction.
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

- Docker with Docker Compose
- Rust 1.89 or newer with Cargo

### 1. Configure the server

```bash
cp .env.example .env
```

Set a private `NEO4J_PASSWORD` in `.env`. The example password is intended only for local development.

### 2. Start Neo4j

```bash
docker compose up -d neo4j
```

Neo4j listens on `bolt://127.0.0.1:7687`; its browser UI is available at `http://127.0.0.1:7474`.
Mindreader does not migrate older graph models. For an existing local development volume, recreate it with `docker compose down --volumes` before starting Neo4j.

### 3. Verify the memory behavior

```bash
cargo run --bin mindreader-smoke
```

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
        "NEO4J_URI": "bolt://127.0.0.1:7687",
        "NEO4J_USER": "neo4j",
        "NEO4J_PASSWORD": "<NEO4J_PASSWORD>"
      }
    }
  }
}
```

Restart the MCP client after changing its configuration. A successful connection exposes the ten tools listed in [MCP tool reference](#mcp-tool-reference).

### npm launcher

Pin the package version in MCP configuration so the client always starts the same release:

```json
{
  "mcpServers": {
    "mindreader": {
      "command": "npx",
      "args": ["-y", "@bnomei/mindreader@0.1.0"],
      "env": {
        "NEO4J_URI": "bolt://127.0.0.1:7687",
        "NEO4J_USER": "neo4j",
        "NEO4J_PASSWORD": "<NEO4J_PASSWORD>"
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

1. Choose the request's `layers` visibility union, then call `memory_search` with the task, topic, or person when you do not have an IRI.
2. Call `memory_get` for a returned IRI when you need the exact node or its immediate neighbors.
3. Call `memory_traverse` when you need typed paths beyond one hop.
4. Call `memory_schema` only when the required Class or Property does not exist.
5. Call `memory_assert` once per durable, future-relevant triple.
6. Search again to verify an important write.
7. After using a retrieved node or relationship, call `memory_feedback` with `strengthen` if it helped or `weaken` if it did not.

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

All tools return structured JSON. Every tool except `memory_schema` requires a `layers` array. In the table, `@layers` is short for that request field.

| Tool | Required input | Optional input and defaults | Purpose |
| --- | --- | --- | --- |
| `memory_search` | `layers` | `text`, `labels`, `limit` (`20`, clamped to `1..100`) | Find current visible facts and ranked `ABOUT` context. The result limit is applied only after Spike, weight, relevance, and deterministic tie-break ranking. |
| `memory_get` | `iri`, `layers` | `hops` (`0`; only `1` includes neighbors) | Fetch a visible node and, optionally, its current visible one-hop relationships. |
| `memory_traverse` | `from`, `layers` | `rels` (all fixed relationships), `depth` (`1`, clamped to `1..3`), `limit` (`50`, clamped to `1..200`) | Walk current visible typed relationships in either direction. |
| `memory_stats` | `layers` | None | Report graph-model readiness, visible node/edge counts, database-wide episode count, and per-membership active edge totals. |
| `memory_assert` | tagged `s`, `p`, tagged `o`, `layers` | `spike`, `contradicts` (`false`) | Add one set-valued triple or merge memberships into its existing relationship identity. |
| `memory_replace` | tagged `s`, `p`, tagged `old`, tagged `new`, `layers` | `spike`, `contradicts` (`false`), `reason` | Move the selected memberships from one exact value to its correction atomically. |
| `memory_retract` | tagged `target`, `layers` | `reason` | Remove selected memberships and soft-close a fact when its last membership is removed. |
| `memory_feedback` | `layers`, tagged `target`, `mode` | None | Apply exactly `+1` (`strengthen`) or `-1` (`weaken`) to a visible node or current relationship's shared weight. |
| `memory_layers` | `layers`, tagged `target` | `add`, `remove` (at least one entry across them) | Audit and atomically edit one node or current relationship's memberships. |
| `memory_schema` | `kind`, plus `name` or `iri` | `subClassOf`, `subPropertyOf`, `domain`, `range` | Declare a Class or Property and its structural links as global records. |

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

### Provenance and history

Each state-changing mutation creates one `Episode` with the tool name and timestamp; no-op mutations create none. Relationships carry the episode identifier. Retraction sets `validTo` and optionally records a reason; it does not delete graph nodes. `CONTRADICTS` and `SUPERSEDES` are system-owned history predicates and cannot be asserted, replaced, or retracted directly.

Ordinary assertions are set-valued: the same `(subject, property, object)` relationship is idempotent apart from membership merges, while a different object becomes another current value. `memory_replace` is the correction operation: it moves only the selected memberships from the requested old fact, preserves unrelated current values and memberships, and creates `SUPERSEDES` history in one Neo4j transaction.

Mutations acquire deterministic graph locks and retry Neo4j transient transaction failures with bounded backoff. Commit failures are never retried because their outcome may be ambiguous.

`CONTRADICTS` is multi-valued. Setting `contradicts: true` on an assertion records explicit links to conflicting visible current objects.

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

Mindreader is privacy-first in the sense that it is self-hosted and sends no memory to an extraction or embedding service. It does not add an application authentication layer in front of MCP or Neo4j, and layers are a relationship filter rather than a security boundary. Anyone who can start or control the MCP process can use its configured Neo4j credential. Secure the client configuration, `.env`, host process, network path, and Neo4j deployment accordingly.

## Configuration reference

Mindreader checks `/workspace/mindreader/.env` and then `.env`, loading values into the process environment. Variables already present in the process environment take precedence over file values.

| Variable | Required | Default | Description |
| --- | --- | --- | --- |
| `NEO4J_PASSWORD` | Yes | None | Neo4j credential. Keep it out of version control and MCP client logs. |
| `NEO4J_URI` | No | `bolt://127.0.0.1:7687` | Bolt or Neo4j endpoint. Compose overrides it with `bolt://neo4j:7687`. |
| `NEO4J_USER` | No | `neo4j` | Neo4j username. |

At first database use, Mindreader marks model version 3, creates required uniqueness constraints and full-text indexes, seeds base schema entities, waits for indexes, and verifies their exact online definitions. Version 3 requires a fresh database. There is no migration path: an unversioned non-empty or incompatible database is rejected with instructions to recreate the Neo4j database or volume.

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
| [`src/tools.rs`](src/tools.rs) | Tool arguments, scoped graph behavior, feedback, membership auditing, supersession, contradiction, and retraction. |
| [`src/graph.rs`](src/graph.rs) | Neo4j connection/bootstrap, graph serialization, safe identifiers, IRI-backed merges, and provenance helpers. |
| [`src/config.rs`](src/config.rs) | Environment loading and defaults. |
| [`src/layers.rs`](src/layers.rs) | Layer validation and visibility-union policy. |
| [`src/iri.rs`](src/iri.rs) | IRI detection, slugging, minting, and kind/label mappings. |
| [`src/bin/mindreader-smoke.rs`](src/bin/mindreader-smoke.rs) | Live end-to-end graph behavior checks. |
| [`scripts/mcp_handshake_probe.py`](scripts/mcp_handshake_probe.py) | Portable MCP initialize and tool-discovery diagnostic. |
| [`mcp.json`](mcp.json) | Server metadata, transport, environment, and exported tool inventory. |

## Troubleshooting

### `NEO4J_PASSWORD is not set`

Create `.env` from `.env.example`, set `NEO4J_PASSWORD`, and restart the server process or MCP client.

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

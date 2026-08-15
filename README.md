# mindreader

Deterministic, privacy-first MCP memory server for Neo4j.

mindreader stores memory as explicit graph triples (`NODE -REL-> NODE`) with:
- no hidden LLM extraction,
- no required embeddings,
- provenance (`Episode`) on writes,
- supersession history (`SUPERSEDES`) instead of destructive overwrites.

It is designed for:
- **solo local-agent workflows** that want durable memory without external SaaS,
- **multi-agent/shared-memory teams** that need a common, inspectable memory graph.

## Core model

- Facts are asserted as `s,p,o` edges (usually `ASSERTS`) with `layer` and `validFrom`.
- Corrections create new current facts and close old ones (`validTo` + `SUPERSEDES`).
- Retract is soft-delete (`validTo`), not hard-delete.
- Reads are layer-aware: only `global` + current project layer are visible.

## Wakeup-first session flow

Use this as a first-class session start pattern:
1. `memory_search` with the task/topic/person to load current context.
2. `memory_get` for specific IRIs returned by search.
3. `memory_traverse` for typed relationship expansion.
4. `memory_assert` only for durable, future-relevant facts.

## Tools

| Tool | Purpose |
| --- | --- |
| `memory_search` | Wakeup retrieval of current visible facts and SPIKE context. |
| `memory_get` | Fetch one node by IRI, optionally with one-hop neighbors. |
| `memory_traverse` | Walk fixed, typed relationships from a starting node. |
| `memory_stats` | Operational counters for nodes/edges/episodes and per-layer activity. |
| `memory_assert` | Insert/update a fact with provenance and supersession semantics. |
| `memory_retract` | Soft-retract current facts (`validTo`), never hard-delete. |
| `memory_schema` | Declare Class/Property schema-as-data entities and links. |

## IRI minting rules

mindreader accepts explicit IRIs or mints them deterministically from names.

| Kind | Prefix pattern | Slug case |
| --- | --- | --- |
| class | `mindreader:class/<slug>` | preserve input case |
| property | `mindreader:property/<slug>` | preserve input case |
| element | `mindreader:element/<slug>` | lowercase |
| literal | `mindreader:literal/<slug>-<hash>` | lowercase |
| episode | `mindreader:episode/<uuid>` | lowercase |
| signal/pattern/insight/knowledge | `mindreader:<kind>/<slug>` | lowercase |

Slug normalization keeps ASCII alnum plus `._-`, converts other runs to `-`, trims dashes, and falls back to `unnamed`.

## Quick start (local Rust)

```bash
cp .env.example .env
# set NEO4J_PASSWORD in .env
cargo run --release --bin mindreader
```

Environment:

```bash
NEO4J_URI=bolt://127.0.0.1:7687
NEO4J_USER=neo4j
NEO4J_PASSWORD=change-me
MINDREADER_PROJECT=project:graph-memory
```

## Docker

Build image:

```bash
docker build -t mindreader:local .
```

Start Neo4j:

```bash
docker compose up -d neo4j
```

Run mindreader over stdio (for MCP clients):

```bash
docker compose run --rm mindreader
```

## Claude Desktop MCP config examples

### Local binary

```json
{
  "mcpServers": {
    "mindreader": {
      "command": "/absolute/path/to/target/release/mindreader",
      "env": {
        "NEO4J_URI": "bolt://127.0.0.1:7687",
        "NEO4J_USER": "neo4j",
        "NEO4J_PASSWORD": "change-me",
        "MINDREADER_PROJECT": "project:graph-memory"
      }
    }
  }
}
```

### Docker Compose

```json
{
  "mcpServers": {
    "mindreader": {
      "command": "docker",
      "args": ["compose", "-f", "/absolute/path/to/docker-compose.yml", "run", "--rm", "mindreader"],
      "cwd": "/absolute/path/to/repo"
    }
  }
}
```

## Minimal end-to-end example (assert → search → traverse)

1. `memory_assert`
   - `s`: `{ "name": "Alice", "labels": ["Element"] }`
   - `p`: `worksOn`
   - `o`: `{ "name": "mindreader", "labels": ["Element"] }`
   - `layer`: `project:graph-memory`

2. `memory_search`
   - `text`: `Alice`

3. `memory_traverse`
   - `from`: `<Alice IRI returned by assert/search>`
   - `depth`: `2`

## Operational notes

- Connection startup includes bounded retry/backoff for transient Neo4j failures.
- `memory_stats` provides a lightweight operator view of graph activity.
- Secrets stay env-only; Cypher uses parameters for user-provided values.

## Smoke test

```bash
cargo run --bin mindreader-smoke
```


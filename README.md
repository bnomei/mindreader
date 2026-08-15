# mindreader

Stdio MCP server that stores agent memory in Neo4j as RDFS schema-as-data: every fact is NODE-REL-NODE. An Element is aboutness/identity, not a SPIKE rung. SPIKE labels (Signal, Pattern, Insight, Knowledge) sit ABOUT an Element. Domain facts use `ASSERTS` edges with `propertyIri` and a `layer`; writes are idempotent and superseding, never in-place clobber, and retract is soft (`validTo`) only.

## Tools

| Tool | Arguments | What it does |
| --- | --- | --- |
| `memory_get` | `iri`, `hops?` (`0` or `1`, default `0`) | Fetch a node; with `hops=1` include current visible neighbors. |
| `memory_search` | `text?`, `labels?`, `limit?` (default 20) | Wake-up: current visible facts (`s,p,o`) plus ABOUT SPIKE, ranked Knowledge > Insight > Pattern > Signal. Not a node directory. |
| `memory_traverse` | `from`, `rels?`, `depth?` (1–3, hard cap 3), `limit?` | Walk the fixed relationship set from a node. |
| `memory_assert` | `s`, `p`, `o`, `layer?`, `spike?`, `contradicts?` | MERGE a triple. Same current triple is a no-op; same `(s,p,layer)` with a new `o` closes the old edge and `SUPERSEDES`. Always returns `conflicts[]` when another visible layer has a different current `o` for `(s,p)`. `contradicts: true` writes multi-valued `CONTRADICTS` (does not supersede a previous fight). `spike` labels `s` and adds `ABOUT` when `o` is an Element. |
| `memory_retract` | `iri?` **or** `s`,`p`,`o?`,`layer?`, plus `reason?` | Soft-retract (`validTo=now`). Nodes stay gettable. Never hard-deletes. |
| `memory_schema` | `kind` (`class` or `property`), `name` or `iri`, `subClassOf?`, `subPropertyOf?`, `domain?`, `range?` | Declare schema nodes and RDFS links. |

There is no Cypher tool. `project_id` is env-only, never a tool argument.

Reads see edges with `validTo IS NULL` and `layer` in (`global`, bound project). Writes accept only those two layers.

## Environment

```
NEO4J_URI=bolt://127.0.0.1:7687
NEO4J_USER=neo4j
NEO4J_PASSWORD=...
MINDREADER_PROJECT=project:graph-memory
```

Copy `.env.example` to `.env`. Password and operator notes live in `.env` and `/workspace/neo4j/STATUS.md` — not here.

## How other bots connect

This is a **stdio MCP** server. Launch the process with the env above; speak MCP over stdin/stdout.

```
cd /workspace/mindreader
cargo run --release --bin mindreader
```

or the built binary:

```
NEO4J_URI=bolt://127.0.0.1:7687 \
NEO4J_USER=neo4j \
NEO4J_PASSWORD=... \
MINDREADER_PROJECT=project:graph-memory \
/workspace/mindreader/target/release/mindreader
```

Do not write logs to stdout (it is the protocol stream).

Live check against Neo4j (same library functions as the tools):

```
cargo run --bin mindreader-smoke
```

## Neo4j on this computer

Bolt: `bolt://127.0.0.1:7687` (HTTP browser `http://127.0.0.1:7474`). Container `neo4j-graph-memory`, Community 2026.07.1.

If it is down, read `/workspace/neo4j/STATUS.md`. `dockerd` may need a manual start, then `sudo docker start neo4j-graph-memory`.

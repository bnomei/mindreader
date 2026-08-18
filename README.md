# Mindreader

[![Crates.io Version](https://img.shields.io/crates/v/mindreader)](https://crates.io/crates/mindreader)
[![Build Status](https://github.com/bnomei/mindreader/actions/workflows/ci.yml/badge.svg)](https://github.com/bnomei/mindreader/actions/workflows/ci.yml)
[![npm](https://img.shields.io/npm/v/@bnomei/mindreader)](https://www.npmjs.com/package/@bnomei/mindreader)
[![License](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Discord](https://flat.badgen.net/badge/discord/bnomei?color=7289da&icon=discord&label)](https://discordapp.com/users/bnomei)
[![Buymecoffee](https://flat.badgen.net/badge/icon/donate?icon=buymeacoffee&color=FF813F&label)](https://www.buymeacoffee.com/bnomei)

**Mindreader gives AI agents a memory they must curate, not a history they can search.**

Most agent-memory systems begin with a conversation or document archive and ask, “How can we retrieve the right passage later?” Mindreader begins with a different question:

> What did this agent learn that is important enough to become durable knowledge?

An agent using Mindreader deliberately keeps a small set of reusable facts, decisions, constraints, preferences, relationships, and lessons. Each memory is an explicit relationship such as `project → uses → Neo4j`. The agent can later correct it, retire it, qualify when it was true, or limit where it is visible.

This makes Mindreader **selective prospective memory**: memory chosen now because it should help future work. It is not a transcript store, an automatic fact extractor, or a generic RAG system.

## Why this is different

The usual promise of agent memory is breadth: retain enough past material that a model can reconstruct an answer later. Mindreader instead optimizes for **precision, maintenance, and trust**.

| Typical transcript or RAG memory | Mindreader |
| --- | --- |
| Retains conversations, chunks, or summaries | Retains deliberately selected knowledge |
| Tries to recover what was said | Recalls what the agent chose to rely on later |
| Retrieval quality depends on finding the right passage | Recall returns explicit entities and relationships |
| New summaries may obscure or replace old ones | Compatible facts coexist; corrections are explicit |
| Provenance often points to a document or message | Every durable assertion change has graph-level history and provenance |
| Broad coverage, including incidental details | Lower coverage by design, with less noise and exposure |

The practical result is a memory that behaves more like a maintained knowledge ledger than an infinite chat history:

- **No facts are captured secretly.** Mindreader never listens to conversations or extracts facts on its own. The agent explicitly decides what to store.
- **Nothing is silently overwritten.** New values can coexist. Replacing a wrong value is a deliberate correction with preserved history.
- **Memory has structure.** Facts connect stable entities instead of living only as isolated text snippets.
- **Memory can express change.** A fact can distinguish when Mindreader learned it from when it was true in the represented world.
- **Memory can be separated by context.** Projects, teams, or tasks can use visibility layers without duplicating the graph.
- **Recall is honest about absence.** No result means no matching assertion was stored and visible—not that something never happened.

Mindreader stores less than archive-first systems. That is the feature and the tradeoff.

## When to use Mindreader

Use Mindreader when an agent needs a durable body of **working knowledge** across sessions and you care about keeping that knowledge understandable and correct.

Good fits include:

- project decisions and why they were made;
- requirements, invariants, conventions, and architectural constraints;
- user preferences and standing instructions;
- identities, responsibilities, ownership, and relationships;
- commitments, blockers, and stable project or environment state;
- reusable operational lessons discovered during work;
- facts whose current value, correction history, or period of validity matters;
- multi-agent work where selected knowledge should be visible in specific contexts.

Mindreader is especially useful when a future agent should be able to ask “What do we currently know and rely on?” rather than “What words appeared somewhere in the past?”

## When not to use Mindreader

Do not use Mindreader as the only memory system when you need:

- complete conversation history or “what did we discuss?” recall;
- exact quotations, original wording, speaker turns, or source excerpts;
- arbitrary question answering over old chats or documents;
- recovery of fleeting details that nobody knew would matter later;
- automatic ingestion with no agent judgment;
- first-query semantic search over a large body of unstructured text;
- hard tenant isolation—the visibility layers are filters, not an authorization boundary.

If every past detail may become answer-bearing, keep the source material in a transcript or document store. If you also need maintained current knowledge, use that archive alongside Mindreader: the archive preserves evidence; Mindreader preserves the conclusions worth carrying forward.

## The deliberate tradeoff

Selective memory reduces noise, duplication, privacy exposure, and the cost of maintaining irrelevant history. Structured facts are easier to inspect, connect, correct, and reuse than generated summaries.

The cost is **extraction loss**. An agent can fail to save an important detail, model it poorly, or fail to combine several memories later. Mindreader cannot recover information that was never asserted. No retrieval algorithm can remove that limitation.

This distinction matters when evaluating memory benchmarks. Conversation-memory tests such as LongMemEval reward systems that retain arbitrary details from source sessions. They measure the whole pipeline—capture policy, model, memory, retrieval, reasoning, and judge—not only the memory engine. Mindreader intentionally chooses a different product boundary, so a raw score on that kind of benchmark should not be presented as pure graph-retrieval quality or silently improved by turning Mindreader into an exhaustive transcript store.

## Semantic recall builds associative trails

Ordinary recall is precise: the agent searches with known names, terms, identities, or relationships. Mindreader's optional semantic recall is for the opposite situation—the agent knows roughly what it needs but does not know the graph's exact vocabulary. It can ask for “the constraints around shipping this project” instead of constructing a detailed query from remembered entity names and predicates.

This is not a conventional vector search over every stored fact. Mindreader uses a query embedding to find **associative trails** back to explicit graph knowledge:

1. A new concept must first connect to facts through strong direct text evidence.
2. Mindreader remembers a small, expiring association between the shape of that query and up to three grounded facts.
3. Later queries with similar meaning can follow the trail even when they use different words.
4. Once grounded facts are found, Mindreader may also return weaker one-hop context from their **memory neighborhood**.

When an associative trail successfully leads to current, visible facts, using it refreshes its lifetime. Useful trails therefore stay warm through repeated work, while unused ones fade away. Over time, an agent develops familiar routes into the parts of the knowledge graph it actually uses—without needing exact queries every time.

That familiarity does not turn repetition into truth. Semantic recall never raises a fact's weight automatically, cannot make retired or hidden knowledge current, and does not let loosely related results recursively teach themselves. Durable facts remain explicit and independently maintainable; the trails are temporary navigation aids around them.

Only the query text is sent to the configured OpenAI or xAI embedding provider; the provider never receives stored facts or the returned memory neighborhood. Semantic recall is optional, and every other Mindreader operation works without an embedding provider.

## How it works

The agent acts as the clerk of an inspectable Neo4j graph:

1. **Recall** when previous knowledge could affect the work.
2. **Do the work** using recalled facts as evidence for exactly what they assert.
3. **Capture selectively** when a durable decision, fact, preference, constraint, commitment, or lesson emerges.
4. **Maintain memory** when knowledge changes: revise a known error, withdraw stale knowledge, preserve compatible alternatives, or merge identities only when they are confirmed to be the same.

The user does not have to say “remember this.” A capable agent can make those calls proactively, while still choosing not to store transient chatter, raw logs, secrets, temporary paths, or facts already represented more reliably in an authoritative file.

Mindreader exposes exactly eight [Model Context Protocol (MCP)](https://modelcontextprotocol.io/) tools:

| Need | Tool |
| --- | --- |
| Retrieve explicit memory | `recall` |
| Follow associative trails from an approximate intent to grounded facts | `recall_semantic` |
| Store selected facts | `write` |
| Correct one exact fact while preserving history | `revise` |
| Retire knowledge without deleting its history | `withdraw` |
| Record whether a retrieval helped or distracted | `judge` |
| Change visibility memberships | `place` |
| Permanently merge two confirmed identities | `unify` |

The reusable [`using-mindreader` agent skill](skills/using-mindreader/SKILL.md) contains the detailed operating contract, request shapes, and maintenance rules. [`mcp.json`](mcp.json) contains the server metadata and tool inventory.

## Start here

| I want to… | Start here |
| --- | --- |
| Install the released server | [Install a release](#install-a-release) |
| Run and verify the project from source | [Quickstart from source](#quickstart-from-source) |
| Register it with an MCP host | [Connect an MCP client](#connect-an-mcp-client) |
| Configure Neo4j or optional semantic recall | [Configuration reference](#configuration-reference) |
| Teach an agent how to use Mindreader | [Agent integration](#agent-integration) |
| Develop, test, or benchmark the server | [Development](#development) |

## Install a release

Choose one installation method. GitHub Release assets cover x64 and arm64 GNU/glibc and musl Linux, x64 and arm64 macOS, and x64 Windows. The npm launcher supports the GNU/glibc, macOS, and Windows assets and requires Node.js 18 or newer. On first use it downloads the matching binary, verifies its SHA-256 checksum, and stores it in a versioned cache.

### npm launcher

```bash
npx -y @bnomei/mindreader@latest --version
```

Pin the package version in MCP configuration so the host starts the same Mindreader release each time. The launcher honors `MINDREADER_VERSION`, `MINDREADER_REPOSITORY`, and `MINDREADER_NPM_CACHE` for version, mirror, and cache overrides.

### Direct installer

GNU Linux and macOS users can install the latest release:

```bash
curl -fsSL https://raw.githubusercontent.com/bnomei/mindreader/main/scripts/install.sh | sh
```

### Cargo

With [cargo-binstall](https://github.com/cargo-bins/cargo-binstall), install the matching prebuilt GitHub Release binary without compiling Mindreader:

```bash
cargo binstall mindreader
```

If `cargo binstall` is not available yet, install it first:

```bash
cargo install cargo-binstall --locked
cargo binstall mindreader
```

To build the published crate from source with Rust 1.89 or newer:

```bash
cargo install mindreader --locked
```

Both Cargo paths install the `mindreader` executable into Cargo's binary directory, normally `$HOME/.cargo/bin`.

Verify a native installation:

```bash
mindreader --version
```

Expected output for this release:

```text
mindreader <version>
```

### Release artifacts

Checksummed archives are available from [GitHub Releases](https://github.com/bnomei/mindreader/releases). Static musl archives for x64 and arm64 Linux are available as manual downloads. The release container is published to GHCR:

```bash
docker run --rm ghcr.io/bnomei/mindreader:latest --version
```

## Quickstart from source

This path runs Neo4j in Docker, verifies the graph behavior, and builds a local MCP server binary.

### Prerequisites

- Docker with Docker Compose. Neo4j must provide matching APOC Core and APOC Extended plugins; the included Compose service installs both.
- Rust 1.89 or newer with Cargo

### 1. Configure the server

```bash
cp .env.example .env
```

Set a private `NEO4J_PASSWORD` in `.env`. The example password is intended only for local development. The smoke test in the next step needs the same value in its process environment.
This repository-level file supplies Docker Compose. A native Mindreader process instead uses its OS configuration directory, described in [Configuration reference](#configuration-reference).

### 2. Start Neo4j

```bash
docker compose up -d neo4j
```

Neo4j listens on `bolt://127.0.0.1:7687`; its browser UI is available at `http://127.0.0.1:7474`.
Mindreader does not migrate older graph models. Recreate a disposable local volume with `docker compose down --volumes` only when Mindreader reports that the database is unversioned, non-empty, or incompatible. This command permanently deletes that Compose volume.

### 3. Verify the memory behavior

```bash
docker compose --profile tools up -d neo4j-tools
set -a
. ./.env
set +a
cargo run --features developer-tools --bin mindreader-smoke -- --config-dir packaging/tools-config
```

The `set -a` block is for POSIX shells and exports the password from the Compose `.env` without putting it in your shell history. The smoke process reads non-secret settings from `packaging/tools-config` (Bolt 7688) so it cannot rewrite a live MCP embedding space on 7687. Process `NEO4J_PASSWORD` still wins over that colocated `.env`.

The smoke test creates test graph data and exercises catalog creation, dynamic multi-layer visibility, membership merging, scoped revision and withdrawal, signed judgment ranking, concurrent judgments, membership auditing, endpoint closure, and stable fact-handle retrieval. A successful run ends with:

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

Mindreader uses stdio transport. It prefers MCP `2026-07-28` (discovery lifecycle and self-contained request metadata) and also accepts initialize `2025-11-25` so hosts that have not adopted the newer version can still list tools. Older initialization protocols are rejected rather than downgraded. The MCP client starts the process and exchanges JSON-RPC messages over its standard input and output. Configure one of the launch methods below, restart the client, and verify that it lists all eight tools shown in [How it works](#how-it-works).

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

Configure the Neo4j URI, username, provider models, and semantic-search policy in the native [`config.toml`](#configuration-reference), not in MCP environment variables. Keep only secrets in the `env` block or native `.env` file.

### npm launcher

Pin the package version in MCP configuration so the client always starts the same release:

```json
{
  "mcpServers": {
    "mindreader": {
      "command": "npx",
      "args": ["-y", "@bnomei/mindreader@0.5.0"],
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

## Agent integration

Give your agent the reusable [`using-mindreader` skill](skills/using-mindreader/SKILL.md). It teaches the agent when to recall, what deserves retention, what must stay out of memory, and how to maintain assertions safely. The user should not need to operate the tools manually.

The short version is the four-step loop described above: recall when prior knowledge matters, do the work, capture only durable outcomes, and explicitly maintain knowledge when it changes.

## Security and trust boundary

Mindreader does not add an application authentication layer in front of MCP or Neo4j, and layers are a relationship filter rather than a security boundary. Anyone who can start or control the MCP process can use its configured Neo4j credential and embedding provider key. Graph operations go only to the configured Neo4j endpoint, but every `recall_semantic` also sends its query text to OpenAI or xAI for embedding. It does not send the stored result bundle. Secure the client configuration, native `.env`, host process, network path, Neo4j deployment, and provider accounts accordingly.

## Configuration reference

When the server starts in MCP mode, Mindreader creates `config.toml` and `.env` without overwriting existing files in the configuration directory:

- Linux and macOS: `${XDG_CONFIG_HOME:-$HOME/.config}/mindreader`
- Windows: `%APPDATA%\mindreader`

The directory is mode `0700` on Unix and the generated `.env` is mode `0600`. Mindreader does not read non-secret settings from process environment variables. Put them in `config.toml`:

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
recall_similarity_threshold = 0.65
convergence_similarity_threshold = 0.90
convergence_result_overlap_threshold = 0.60
rrf_k = 15.0
direct_weight = 2.0
keyword_weight = 0.5
```

### Non-secret settings

| Setting | Type | Default | Constraint or effect |
| --- | --- | --- | --- |
| `neo4j.uri` | string | `bolt://127.0.0.1:7687` | Neo4j Bolt endpoint. |
| `neo4j.user` | string | `neo4j` | Neo4j username. |
| `embeddings.openai.model` | string | `text-embedding-3-small` | Must be non-empty when `OPENAI_API_KEY` selects OpenAI. |
| `embeddings.openai.dimensions` | integer | `1536` | Must be `1..4096` for the selected provider and match returned vectors. |
| `embeddings.xai.model` | string | empty | Must be set when `XAI_API_KEY` selects xAI. |
| `embeddings.xai.dimensions` | integer | `1536` | Must be `1..4096` for the selected provider and match returned vectors. |
| `semantic.ttl_days` | integer | `30` | Must be positive; controls semantic activation expiry. |
| `semantic.neighbor_limit` | integer | `10` | Must be `1..100`; bounds vector neighbors considered for recall. |
| `semantic.recall_similarity_threshold` | number | `0.65` | Finite value in `[0, 1)`; activation evidence is normalized above this floor. |
| `semantic.convergence_similarity_threshold` | number | `0.90` | Finite value in `0..1`. |
| `semantic.convergence_result_overlap_threshold` | number | `0.60` | Finite value in `0..1`. |
| `semantic.rrf_k` | number | `15.0` | Must be finite and positive. |
| `semantic.direct_weight` | number | `2.0` | Must be finite and greater than `keyword_weight`; weights exact direct matches. |
| `semantic.keyword_weight` | number | `0.5` | Must be finite and positive; weights keyword-only direct matches. |

The TOML parser rejects unknown fields. See [`src/config.rs`](src/config.rs) for the defaults, validation, and provider-selection contract.

### Secrets and precedence

The colocated `.env` contains only secrets:

```dotenv
NEO4J_PASSWORD=
OPENAI_API_KEY=
XAI_API_KEY=
```

| Secret | Required | Behavior |
| --- | --- | --- |
| `NEO4J_PASSWORD` | Yes, for database-backed calls | Authenticates the configured Neo4j user. MCP initialization and `tools/list` remain available without a successful database connection. |
| `OPENAI_API_KEY` | No | Enables semantic search with `[embeddings.openai]`. Takes precedence when both provider keys are set. |
| `XAI_API_KEY` | No | Enables semantic search with `[embeddings.xai]` when no OpenAI key is set. |

A non-empty process environment secret takes precedence over the colocated `.env` value. Empty process values fall back to non-empty file values. Provider failures do not fall through to another provider because the configured vector spaces may differ.

### Database bootstrap

At first database use, Mindreader requires matching APOC Core and APOC Extended installations, verifies the text-similarity, node-merge, and TTL functions and procedures, then marks model version 9, creates required uniqueness, full-text, and optional vector indexes, seeds base schema entities, and waits for indexes. The merge-candidate index uses a synchronous keyword-analyzed lowercase whole-name property so APOC can rerank a small indexed candidate set without scanning every entity. APOC TTL must be enabled. The included Docker Compose configuration installs both plugins and grants only the required APOC surface.

Initializing model version 9 requires an empty database. Subsequent starts accept a compatible model-v9 database. There is no migration path: an unversioned non-empty or incompatible database is rejected with instructions to recreate the Neo4j database or volume.

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

The second command waits for MCP input on stdin without allocating a pseudo-TTY. Serving-mode diagnostics go to stderr; stdout remains reserved for MCP messages.

## Development

Install [Just](https://github.com/casey/just#installation) 1.58.0 or newer, then use the fast compile and test recipes while developing:

```bash
just check
just test
```

Before handoff, run `just verify-full`. It applies the same formatting, warning-free Clippy, and locked all-target/all-feature test gate used by CI and release verification.

Cargo artifacts remain in the repository's normal `target/` directory. If Cargo's apparent-size summary grows beyond 5 GiB, inspect it with `just target-report`, preview every managed path with `just clean-preview`, and run an appropriately scoped `cargo clean` only after reviewing the preview. Both reporting recipes use Cargo's cross-platform dry-run mode; cleanup is never automatic.

Run the live integration smoke test when Neo4j is available:

```bash
docker compose --profile tools up -d neo4j-tools
cargo run --features developer-tools --bin mindreader-smoke -- --config-dir packaging/tools-config
```

Pass `--config-dir PATH` after `--` to use an isolated native `config.toml` and `.env` instead of the operator's normal configuration directory. The smoke test writes persistent fixtures to the configured Neo4j database and does not clean them up. Do not run smoke or bench against a live MCP database on port 7687. See [`CONTRIBUTING.md`](CONTRIBUTING.md) for graph model versioning and release expectations.

Run the release-mode graph benchmark against a disposable database before changing search, logical locks, or merge-candidate generation:

```bash
cargo run --release --features developer-tools --bin mindreader-bench -- --config-dir packaging/tools-config --entities 10000 --samples 30
```

The benchmark refuses any database that is not pristine after model-v9 bootstrap, seeds persistent fixtures, validates the exact search order against its deterministic oracle, and reports nearest-rank latency distributions for common-hit search, batched logical locks, and merge suggestions at 1/4/20 newly created entities. Never point it at production or a database whose contents must be preserved.

Use Divan for graph-free CPU regressions:

```bash
cargo bench --features developer-tools --bench cpu_hotspots
```

The normalization matrix covers dimensions 3, 256, 512, 1536, 3072, and 4096. Keep array work sequential unless a supported workload demonstrates a parallel crossover without changing deterministic output.

## Repository map

| Path | Responsibility |
| --- | --- |
| [`src/server.rs`](src/server.rs) | MCP tool registration, advertised input schemas, protocol negotiation, and lazy database access. |
| [`src/service.rs`](src/service.rs) | Typed application boundary used by transport adapters. |
| [`src/error.rs`](src/error.rs) | Typed application errors, retained source context, and Neo4j retry classification. |
| [`src/domain.rs`](src/domain.rs) | Validated layer IDs and tagged entity, literal, target, revision, and withdrawal concepts. |
| [`src/tools.rs`](src/tools.rs) | Mutation arguments, scoped graph behavior, judgment, membership auditing, supersession, contradiction, and withdrawal. |
| [`src/search.rs`](src/search.rs) | Full-text and label-only retrieval, complete database-side ranking, and bounded result assembly. |
| [`src/merge.rs`](src/merge.rs) | Permanent entity merging and APOC-backed fuzzy suggestions. |
| [`src/semantic.rs`](src/semantic.rs) | Direct-plus-vector semantic ranking, activation convergence, and TTL refresh. |
| [`src/embeddings.rs`](src/embeddings.rs) | Native OpenAI and xAI embedding clients and vector validation. |
| [`src/graph.rs`](src/graph.rs) | Neo4j connection/bootstrap, graph serialization, safe identifiers, vector-index setup, and provenance helpers. |
| [`src/config.rs`](src/config.rs) | Native TOML and secret-file initialization, validation, provider selection, and defaults. |
| [`src/layers.rs`](src/layers.rs) | Layer validation and visibility-union policy. |
| [`src/iri.rs`](src/iri.rs) | IRI detection, slugging, minting, and kind/label mappings. |
| [`src/bin/mindreader-smoke.rs`](src/bin/mindreader-smoke.rs) | Live end-to-end graph behavior checks. |
| [`src/bin/mindreader-bench.rs`](src/bin/mindreader-bench.rs) | Reproducible release-mode search, logical-lock, and merge-suggestion benchmarks. |
| [`benches/cpu_hotspots.rs`](benches/cpu_hotspots.rs) | Divan microbenchmarks for graph-free CPU paths and supported workload bounds. |
| [`scripts/mcp_handshake_probe.py`](scripts/mcp_handshake_probe.py) | Portable MCP discovery, protocol-rejection, and tool-inventory diagnostic. |
| [`mcp.json`](mcp.json) | Server metadata, transport, environment, and exported tool inventory. |

## Troubleshooting

### `NEO4J_PASSWORD is not set`

Set `NEO4J_PASSWORD` in the `.env` beside the generated `config.toml`, or in the MCP process environment, then restart the server process or client. The repository `.env.example` is for Docker Compose rather than native config discovery.

### Semantic search reports that no provider is configured

Set `OPENAI_API_KEY` or `XAI_API_KEY` in the native `.env` or process environment. If using xAI, also set a non-empty `[embeddings.xai].model` and the matching dimensions in `config.toml`. Provider failure does not fall through to the other provider because their vector spaces are not interchangeable.

Embedding requests make at most three attempts within a fixed 20-second operation budget. Retryable transport failures and HTTP `408`, `409`, `429`, and server errors use jittered backoff; provider `Retry-After` guidance is honored only when it fits the remaining budget. Error and success bodies are size-bounded before parsing.

### A database call reports a missing APOC function or procedure

Install APOC Core and APOC Extended versions matching Neo4j, enable APOC TTL, and allow the text, merge, and TTL calls shown in `docker-compose.yml`. Recreate a development database after changing an incompatible graph model.

### Neo4j connection failures

Check the container and credentials:

```bash
docker compose ps
docker compose logs neo4j
```

Mindreader uses the configured URI exactly as written and connects lazily on the first database-backed call. MCP discovery and `tools/list` can still succeed while Neo4j is unavailable; that first database-backed call reports the connection error.

### The MCP client connects but lists no tools

Confirm that the configured command uses an absolute executable path, the process can read its environment, and stdout is not redirected through a wrapper that prints status messages. Mindreader writes its own diagnostics to stderr.

### A request reports an invalid layer

Use lowercase kebab-case segments separated by colons, for example `project:graph-memory`. Pass `scope: []` when only global records should participate.

## License

Mindreader is available under the [MIT License](LICENSE).

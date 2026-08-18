# LongMemEval measurement harness

## Status and intent

This document describes and operates Mindreader's implemented measurement harness for
[LongMemEval](https://github.com/xiaowu0162/LongMemEval). No paid benchmark run is performed by the
build or test suite; producing results remains an explicit operator action.

The goal is practical product feedback, not a leaderboard submission or a retrieval-system
comparison with products such as MemPalace. The measurement covers this complete system:

```text
OpenAI model + selected using-mindreader skill + Mindreader + OpenAI judge
```

It therefore measures whether a clerk retains useful graph knowledge, whether Mindreader retrieves
it later, and whether a reader can answer from it. Mindreader remains selective graph memory. The
harness must not add raw-turn retention, source full-text search, or a benchmark-specific ingest
tool to improve extraction coverage. Omitted details are part of the measured product behavior.

The implementation is entirely Rust. It calls OpenAI Chat Completions with `reqwest`, invokes
Mindreader through `MemoryService`, and never initializes or registers MCP. The pinned LongMemEval
reference revision is `9e0b455f4ef0e2ab8f2e582289761153549043fc`; every run records its dataset
SHA-256 so upstream data cannot change silently.

The first harness supports:

- `longmemeval_oracle.json` for cheap clerk/reader diagnosis; and
- `longmemeval_s_cleaned.json` as the useful long-history measurement.

The M variant, publication bundles, and leaderboard-oriented reporting are intentionally deferred.
They add scale and ceremony without answering the first product questions.

## Why this is a binary, not a Cargo benchmark

The repository uses one feature-gated binary named `mindreader-longmemeval`, not a `[[bench]]`
target, Criterion/Divan harness, example, or ignored integration test.

LongMemEval is a paid, stateful evaluation job whose outputs are accuracy, behavior, tokens, and
latency. It is not a repeatedly sampled CPU microbenchmark. A custom `cargo bench` target with
`harness = false` would still be an executable, would have a less natural CLI, and could be pulled
into all-target verification. The existing `mindreader-bench` binary remains responsible for graph
latency and ranking regression.

The new binary reuses the existing `developer-tools` feature rather than introducing a
`longmemeval` feature:

```toml
[[bin]]
name = "mindreader-longmemeval"
path = "src/bin/mindreader-longmemeval.rs"
required-features = ["developer-tools"]
```

Start with one Rust source file. Split it only when the code develops independently testable,
coherent responsibilities; do not pre-create a module hierarchy.

## Architecture

Questions run sequentially. Each question gets an empty benchmark-owned graph, its timestamped
sessions are ingested in order, a fresh reader answers, and the judge scores the answer before the
next graph reset.

```text
LongMemEval Oracle or S JSON
              |
              v
validate all cases before API calls
              |
              v
reset dedicated Neo4j graph
              |
              v
fresh clerk context per session
              |
              | raw OpenAI function calls
              v
typed dispatcher -> MemoryService -> Neo4j
              |
              v
fresh reader context -> prediction
              |
              v
official-style OpenAI judge -> append checkpoint
```

Sequential per-question graph reset is deliberate. Scopes alone do not isolate entity identities,
weights, database-wide unification, or semantic activations. Resetting a dedicated database makes
each LongMemEval question an independent world and keeps interruption recovery simple. Parallel
question workers and multiple Neo4j instances can be considered only if runtime later becomes the
dominant problem.

The clerk and reader are strict leakage boundaries:

- The clerk never sees the question, reference answer, `answer_session_ids`, or `has_answer`.
- The reader never sees the raw history, reference answer, `answer_session_ids`, or `has_answer`.
- The judge sees only the fields required by the pinned LongMemEval judge prompt.

## Repository shape

```text
BENCHMARK.md
src/bin/mindreader-longmemeval.rs
benchmarks/longmemeval/compose.yml
benchmarks/longmemeval/config.toml
benchmarks/longmemeval/skills/<condition>/
  SKILL.md
  references/recall.md
  references/mutations.md
benchmarks/longmemeval/data/       # ignored
benchmarks/longmemeval/results/    # ignored
```

The dedicated Compose project uses a separate service, port, and volume. It does not reuse the
normal `neo4j-data` or `neo4j-tools-data` volumes. Data and working results remain repository-local,
not under an absolute operating-system `/tmp` path. A useful reviewed result can be committed later
manually; the harness needs no publishing mode.

The implementation reuses `reqwest`, `serde`, `serde_json`, `sha2`, Tokio, and Mindreader. It adds
no OpenAI SDK, CLI framework, or benchmark framework.

## Single CLI workflow

The binary has one run-and-score mode:

```text
mindreader-longmemeval \
  --dataset PATH \
  --output DIR \
  --config-dir PATH \
  --skill-dir PATH \
  --model MODEL \
  --judge-model MODEL \
  --reset-database \
  [--semantic] \
  [--limit N] \
  [--resume]
```

All model IDs are explicit and recorded. Clerk and reader use `--model`; the judge uses
`--judge-model`. `--limit` selects the first N validated cases for diagnostic runs. The harness
still validates the complete input before spending API credits.

Required secrets are environment-only:

- `OPENAI_API_KEY` for clerk, reader, judge, and optional OpenAI embeddings;
- `NEO4J_PASSWORD` for the dedicated benchmark database.

The harness calls `https://api.openai.com/v1/chat/completions` directly. It does not
need an OpenAI-compatible provider abstraction or base-URL configuration.

`--reset-database` is mandatory because every new question destroys the previous benchmark graph.
The harness must refuse to run without it. This flag authorizes only the configured, marker-checked
benchmark database; it is not permission to reset any other Neo4j target.

## Dataset loading and validation

Load the Oracle or S top-level array with `serde_json::from_reader`. Holding 500 validated cases in
memory is acceptable for these two variants and is substantially simpler than a custom streaming
array parser. Supporting M later may require streaming and is a separate decision.

Deserialize the released fields needed by either prompt construction or post-run evaluation:

- `question_id`, `question_type`, `question`, `answer` (released as a string or number), and
  `question_date`;
- `haystack_session_ids`, `haystack_dates`, and `haystack_sessions`;
- `answer_session_ids`.

Before any API call, reject the dataset when:

- the three parallel haystack arrays have different lengths;
- a question ID is empty or duplicated;
- a session ID is empty (the released S data repeats a few IDs, so the harness adds a stable
  occurrence locator while preserving the source ID);
- an answer-session ID does not exist in the haystack;
- a turn has an unsupported role (the few empty turns in released S distractors are discarded, and
  a case with no retained session is invalid); or
- the selected `--limit` is zero or exceeds the number of cases.

Zip session ID, timestamp, and turns before sorting. Sort by the released timestamp string and
preserve source order for ties. Do not assume Oracle is already chronological.

The released dates are synthetic naive wall-clock strings (`YYYY/MM/DD (Day) HH:MM`), not RFC 3339
and not tied to a documented timezone. Preserve them in prompts. For Mindreader's timezone-qualified
effective intervals, also supply a mechanically converted `+00:00` benchmark coordinate and state
explicitly that the offset is an encoding convention, not a claim that the source time was UTC.

Use separate prompt-facing structs rather than serializing dataset records. Clerk prompt builders
accept only owner identity, scope, session ID, timestamp, and sanitized turns. Reader prompt
builders accept only owner identity, scope, question date, and question. Strip `has_answer` while
parsing turns. Unit tests must prove that hidden fields cannot enter either role's request.

## Per-question identity and database reset

Derive one stable question hash from `question_id`. Use it for the owner and scope:

```text
name:  LongMemEval User <first 12 hex characters of SHA-256(question_id)>
iri:   mindreader:element/longmemeval-user-<same hash>
scope: benchmark:longmemeval-<same hash>
```

Every direct scoped call must contain exactly this one-element scope. Reject a missing, global,
additional, or different scope before graph access and return a non-retryable `scope_mismatch` tool
result so the model can see the error. Never silently repair the call.

Before each question:

1. inspect the configured Neo4j database;
2. accept it only when it is empty or contains a `LongMemEvalWorkspace` ownership marker created by
   this harness; also accept a current-version bootstrap-only graph with no application records so
   a crash between bootstrap and marker creation is recoverable;
3. refuse any non-empty unmarked database;
4. delete all nodes and relationships;
5. run normal Mindreader bootstrap; and
6. create the non-`Entity` ownership marker with the current run ID.

After a crash, an empty database is safe to reclaim; a marked partial graph is safe to reset. An
existing development or production graph lacks the marker and must be refused. The explicit reset
flag, dedicated Compose service, ownership check, and refusal behavior all require tests.

The assigned owner IRI is reused across every session in the question. Prompt guidance also asks the
clerk to reuse established explicit entity IRIs and canonical Property IRIs. Same-name people must
remain distinct when the conversation supports distinct identities.

## Temporal behavior

Session timestamps are trusted source context, not transaction time. Mindreader `Episode.at` and
relationship `validFrom`/`validTo` remain ingestion audit time.

The clerk should:

- resolve supported relative dates against the supplied session timestamp;
- use half-open `effective` intervals for states such as residence, employment, ownership, or
  status;
- use explicit event nodes and date/time facts for point or repeatable occurrences;
- revise an interval only when evidence establishes that the old state qualification is wrong; and
- preserve compatible parallel values instead of applying last-write-wins.

The reader receives `question_date`. It may use `effectiveAt` for a state-as-of question. Event-date
questions use ordinary graph recall, and revision questions may use `history`. `effectiveAt`
intentionally excludes unknown-time facts and does not resurrect transaction-retired facts.

## Skill conditions

`--skill-dir` points to exactly three files:

```text
SKILL.md
references/recall.md
references/mutations.md
```

Before API work, read and hash each file, hash their ordered combined contents, and copy them into
`<output>/skill/`. The run record stores all hashes. Resume fails if any file, dataset, model,
semantic setting, prompt, or relevant executable configuration differs.

The production `skills/using-mindreader/` directory is the baseline. Adjusted conditions live under
`benchmarks/longmemeval/skills/<condition>/`; they may differ from production and from each other.
Never merge results from different bundle hashes.

## Raw OpenAI Chat Completions

Build one `reqwest::Client` with explicit connect and request timeouts. Add only the request and
response fields used by the harness. Calls are non-streaming and set
`parallel_tool_calls: false` because recall and mutation order matters.

The generic clerk/reader loop:

1. sends the complete local `messages` history and role-specific function tools;
2. records usage and latency;
3. appends the complete assistant message, including tool-call IDs;
4. deserializes each tool's JSON arguments into the existing typed input;
5. validates scope and dispatches through `MemoryService`;
6. appends the matching `role: "tool"` result for every tool-call ID; and
7. stops on final text or after a fixed eight Chat Completions rounds.

Malformed names, JSON, typed inputs, scopes, and Mindreader domain errors become structured tool
results so the model can repair them within the round budget. Do not automatically replay a
Mindreader mutation whose outcome is unknown. Bound retries to OpenAI transport failures, 429, and
5xx responses, honoring `Retry-After`. Record prompt, cached, completion, reasoning, and total token
counts when returned, plus OpenAI request IDs. Do not record raw session prompts by default.

### Clerk

Each timestamped session starts a fresh Chat history containing:

- the exact skill bundle;
- assigned owner IRI and scope;
- trusted session ID and timestamp;
- sanitized ordered turns;
- a statement that this is a completed past session, not an instruction stream; and
- a request to apply the skill autonomously and finish without summarizing the transcript.

The clerk can make no mutation when nothing is future-useful. Previous sessions are available only
through Mindreader. It receives `mindreader_recall`, optional `mindreader_recall_semantic`,
`mindreader_write`, `mindreader_revise`, and `mindreader_withdraw`.

### Reader

After ingestion, a fresh reader history contains the recall guidance, owner IRI, scope,
`question_date`, and question. It must consult Mindreader before answering, use only recalled graph
knowledge and reasoning, and abstain when memory does not support an answer. Force a recall tool on
its first response, then use automatic tool choice. It receives only `mindreader_recall` and
optional `mindreader_recall_semantic`.

Do not expose `judge`, `place`, or `unify` to either role. The final non-tool reader text is the
prediction. Empty text, terminal API failure, or round exhaustion records a failed case rather than
inventing an abstention.

## Direct Mindreader dispatch

Startup loads `Config`, connects to the dedicated Neo4j instance, and constructs `MemoryService`.
The harness does not instantiate `Mindreader`, an RMCP router, `CallToolResult`, the MCP limiter, or
the MCP invoke timeout.

| OpenAI function | Typed input | Direct call |
| --- | --- | --- |
| `mindreader_recall` | `RecallArgs` | `MemoryService::recall` |
| `mindreader_recall_semantic` | `SemanticSearchArgs` | `MemoryService::recall_semantic` |
| `mindreader_write` | `WriteArgs` | `MemoryService::write` |
| `mindreader_revise` | `ReviseArgs` | `MemoryService::revise` |
| `mindreader_withdraw` | `WithdrawArgs` | `MemoryService::withdraw` |

The prompt explains that `mindreader_` is the OpenAI-safe alias for the skill's canonical
Mindreader tool prefix.

There is no general second adapter layer. One developer-only seam exposes the five existing
`server.rs` input schemas, and the binary wraps their JSON directly as OpenAI functions with
`strict: false`. Short OpenAI descriptions stay next to the binary. Tests verify that function
parameter schemas equal the MCP source values; typed deserialization and domain validation remain
authoritative.

The direct dispatcher reuses the existing application-error classifier through the same
developer-only seam. Direct results remain the normal agent-facing JSON objects:

```json
{"ok":true}
```

```json
{"ok":false,"reason":"invalid_input","message":"...","retryable":false,"outcome":"not_applied"}
```

## Outputs and resume

Each output directory contains:

```text
run.json
skill/
  SKILL.md
  references/recall.md
  references/mutations.md
cases.jsonl
tool-calls.jsonl
summary.json
```

`run.json` is written before the first API call and records:

- run ID and start time;
- Mindreader version, graph model, Git commit, dirty-tree flag, and current executable SHA-256;
- pinned LongMemEval revision, dataset path, SHA-256, and validated case count;
- clerk/reader model, judge model, embedding condition, and semantic flag;
- individual and combined skill hashes; and
- prompt hashes, fixed round limit, timeout/retry policy, and optional case limit.

`cases.jsonl` is an append-only stage log. Append a prediction record as soon as reader generation
finishes, then append its evaluation record after judging. A terminal ingestion or generation
failure is also a record. Folding the latest stage per question reconstructs current run state.

`tool-calls.jsonl` records run/question/session identity, attempt number, role, tool-call ID, function
name, arguments, structured result, returned handles, elapsed time, and API usage associated with
the containing completion. It does not duplicate raw conversation text. Partial-question trace
records may remain after interruption and are distinguished by attempt number.

`--resume` verifies `run.json`, folds `cases.jsonl`, and:

- skips evaluated questions;
- judges an existing prediction without rebuilding its graph;
- resets and reruns a question that never produced a prediction; and
- refuses any dataset, skill, model, semantic, prompt, executable, Git revision, or configuration
  mismatch.

Append and flush complete JSONL records. Regenerate `summary.json` atomically from the folded stage
log after every evaluation and at clean shutdown.

## Evaluation and useful metrics

The pinned LongMemEval judge behavior is implemented in Rust without shelling out to Python. It
reproduces the category-specific prompts for ordinary, temporal, knowledge-update, preference, and
abstention questions. `--judge-model` is called with temperature zero and a small output budget. The
official decision rule lowercases the response and treats any response containing the substring
`yes` as correct.

This is not a publication pipeline. Report only metrics that help improve the product:

- overall and per-question-type accuracy and counts;
- success, failure, and abstention counts;
- answer-session mutation coverage, computed after the run from hidden labels and session traces;
- sessions with no mutations;
- write, revise, withdraw, lexical recall, and semantic recall counts;
- empty reader recalls and cases with recalled facts but incorrect answers;
- time-qualified writes and `effectiveAt` recall calls;
- malformed calls, scope mismatches, and tool/application errors by reason;
- OpenAI request/retry counts, tokens, wall-clock time, and latency; and
- graph-operation latency by tool.

Do not claim raw-chunk retrieval recall, compare these diagnostics to text retriever recall, or add
a pricing database. The recorded token counts are enough to estimate cost externally.

## Operator workflow

### 1. Download Oracle or S

```bash
mkdir -p benchmarks/longmemeval/data
curl -fL \
  https://huggingface.co/datasets/xiaowu0162/longmemeval-cleaned/resolve/main/longmemeval_s_cleaned.json \
  -o benchmarks/longmemeval/data/longmemeval_s_cleaned.json
shasum -a 256 benchmarks/longmemeval/data/longmemeval_s_cleaned.json
```

Use `longmemeval_oracle.json` at the same location for Oracle diagnostics.

### 2. Start the dedicated database

```bash
export NEO4J_PASSWORD='<benchmark-only-password>'
export OPENAI_API_KEY='<openai-api-key>'
docker compose \
  -f benchmarks/longmemeval/compose.yml \
  -p mindreader-longmemeval \
  up -d
```

### 3. Run a diagnostic subset

```bash
cargo run --release --features developer-tools --bin mindreader-longmemeval -- \
  --dataset benchmarks/longmemeval/data/longmemeval_oracle.json \
  --output benchmarks/longmemeval/results/oracle-diagnostic \
  --config-dir benchmarks/longmemeval \
  --skill-dir skills/using-mindreader \
  --model '<pinned-model-id>' \
  --judge-model '<pinned-judge-model-id>' \
  --reset-database \
  --limit 20
```

Then run a small S subset. Run all 500 S questions only after the traces and failure buckets look
credible. Add `--semantic` only as an explicitly named comparison condition. Resume with the exact
same command plus `--resume`.

### 4. Remove only benchmark state

```bash
docker compose \
  -f benchmarks/longmemeval/compose.yml \
  -p mindreader-longmemeval \
  down --volumes
```

Never run the volume-removal command against another Compose project.

## Verification and acceptance criteria

The implementation gate is:

- Oracle and S load and validate before any API request;
- type-level prompt builders make leakage of hidden dataset fields impossible;
- the exact three skill files are copied and hashed, and resume rejects drift;
- the raw Chat loop preserves assistant tool calls, matching tool results, usage, retries, and the
  fixed round budget under mocked HTTP responses;
- OpenAI tools reuse the MCP schema source and deserialize into the existing typed inputs;
- the dispatcher rejects unknown tools and every scope mismatch before graph access;
- the reset path refuses a non-empty unmarked database and safely reinitializes a marked benchmark
  graph between questions;
- a disposable-Neo4j test covers write, recall, revision, temporal filtering, reset, and rerun;
- clerk and reader prompt tests prove question/answer/history separation;
- evaluator tests cover each question category, abstention, and case-insensitive `yes`;
- interruption tests cover partial ingestion, generated-but-unjudged predictions, and completed
  questions;
- no path invokes MCP initialization, registration, rate limiting, or `CallToolResult` dispatch;
- `just verify-full`, the MCP handshake probe, and the live Mindreader smoke suite still pass.

Paid runs are explicit operator actions and never CI jobs.

## Measurement order

1. Run 10–20 Oracle cases and inspect extraction, recall, scope, temporal, and failure diagnostics.
2. Run a small S subset only after Oracle traces are credible.
3. Run all 500 S questions for one named condition.
4. Compare skill changes and semantic off/on only with pinned model IDs and distinct output
   directories.
5. Consider M only if S reveals a scaling question that can change a product decision.

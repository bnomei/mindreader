# Recall reference

Read this file when selecting a recall mode or interpreting a recall result.

## Select the narrowest mode

| Need | Call |
| --- | --- |
| Search names or terms | `mindreader:memory_recall` with `text` |
| Fetch known node IRIs | `mindreader:memory_recall` with `iris` |
| Inspect Class or Property vocabulary | `mindreader:memory_recall` with `labels: ["Class"]` or `["Property"]` |
| Walk relevant relationships from a node | `mindreader:memory_recall` with `around`, optional `p`, and `depth` |
| Inspect current plus historical facts | `mindreader:memory_recall` with `history` |
| Match meaning after lexical recall is insufficient | `mindreader:memory_recall_semantic` with `text` and optional `labels` |

Use exactly one `mindreader:memory_recall` selector: `text`, `iris`, `labels`, `around`, or `history`.

- Both recall tools default `limit` to 20 and accept `1..=100`. For ordinary recall, `limit` is the returned fact budget except that catalog mode applies it to catalog nodes; semantic recall uses it as the maximum fused results.
- Both accept `detail`: `detailed` is the default full envelope; `concise` returns handles plus thin subject/predicate/object lines.
- `iris` accepts 1–20 unique node IRIs, preserves input order, and reports misses through `lookups[i].found`. `hops` is `0` or `1` and defaults to `0`; incident fact handles still appear at zero hops.
- `around` requires a node IRI. Its `p` filter and `depth` (`1..=3`, default `1`) apply only to this selector. `paths[i]` is the deterministic shortest witness path for `facts[i]`.
- `history` accepts one node or fact IRI and returns current and `validTo`/`SUPERSEDES` history for that identity.
- Do not pass a fact IRI (`mindreader:relationship/...`) to `iris` or `around`.
- Text, non-catalog `labels`, and semantic recall also populate `nodes[]` from fact endpoints.
- Current facts expose `rateable` and `mutable` for the request scope. A global record visible through a named scope is rateable but not mutable there.

Ordinary recall stays inside Neo4j and is read-only. Semantic recall sends only the query text, up to 32 KiB UTF-8, to the configured embedding provider and maintains expiring semantic activations. Use it only when conceptual matching is necessary and that disclosure is acceptable.

```json
{
  "text": "Alice",
  "scope": ["project:graph-memory", "team:agents"],
  "detail": "concise"
}
```

# Recall reference

Read this file when selecting a recall mode or interpreting a recall result.

Recall is agent-driven. Use it proactively when starting, resuming, or deciding work that could be changed by earlier decisions, preferences, constraints, identities, relationships, conventions, commitments, project state, or lessons. Do not wait for the user to ask what is remembered, but do not recall speculatively when prior context cannot affect the task.

## Select the narrowest mode

| Need | Call |
| --- | --- |
| Search names or terms | `mindreader:recall` with `text` |
| Fetch known node IRIs | `mindreader:recall` with `iris` |
| Inspect Class or Property vocabulary | `mindreader:recall` with `labels: ["Class"]` or `["Property"]` |
| Walk relevant relationships from a node | `mindreader:recall` with `around`, optional `p`, and `depth` |
| Inspect current plus historical facts | `mindreader:recall` with `history` |
| Match meaning after lexical recall is insufficient | `mindreader:recall_semantic` with `text` and optional `labels` |

Use exactly one `mindreader:recall` selector: `text`, `iris`, `labels`, `around`, or `history`.

- Both recall tools default `limit` to 20 and accept `1..=100`. For ordinary recall, `limit` is the returned fact budget except that catalog mode applies it to catalog nodes; semantic recall uses it as the maximum fused results.
- Both accept `detail`: `detailed` is the default operation and audit envelope; `concise` is answer-only. Concise facts retain subject/predicate/object identity and display content plus non-null `spike`, but omit handles, memberships, ranking, weights, mutability, and rateability. It also omits echoed mode/scope, endpoint-derived `nodes[]`, and `about[]` while retaining truncation and selector-specific answers. Compact around witness paths remain available without relationship IRIs; history retains `current`, `validTo`, and exact revisions; catalog retains its nodes; IRI mode retains ordered lookup status and incident facts.
- `iris` accepts 1–20 unique node IRIs, preserves input order, and reports misses and per-lookup truncation through `lookups[i]`. `hops` is `0` or `1` and defaults to `0`; incident facts still appear on `lookups[i].facts`. `limit` is per requested IRI. `hops: 1` also fills top-level `facts[]` when `detail` is `detailed`; concise keeps the answer in ordered lookups without duplicating it.
- `around` requires a node IRI. Its `p` filter, `direction` (`both`|`outgoing`|`incoming`, default `both`), and `depth` (`1..=3`, default `1`) apply only to this selector and constrain every traversed edge. `paths[i]` is the deterministic hub-aware witness path for `facts[i]`, with `{iri,from,p,to}` edges in detailed mode and `{from,p,to}` edges in concise mode.
- `history` accepts one node or fact IRI and returns current and `validTo` facts plus newest-first exact `revisions[]` events. In detailed mode, use their previous/replacement fact handles normally; `supersedes` is audit metadata and is never pasteable.
- Do not pass a fact IRI (`mindreader:relationship/...`) to `iris` or `around`.
- Detailed text, non-catalog `labels`, and semantic recall also populate `nodes[]` from fact endpoints.
- Detailed current facts expose `rateable` and `mutable` for the request scope. A global record visible through a named scope is rateable but not mutable there.
- `ABOUT` is explicit context. Detailed text recall may return classified context in top-level `about[]`; `ABOUT` never appears in ordinary `facts[]`, neighborhood/history facts, or semantic activation bundles.

Text recall safely combines an escaped exact phrase with bounded OR-keyword matching, so a natural-language query can surface facts through overlapping terms without requiring the whole sentence to occur verbatim. Keyword fallback keeps only unique Unicode-alphanumeric terms of at least four characters, avoiding language-specific stopword tables; shorter terms can still match through the exact phrase. A relationship's indexed fact text supplies its primary lexical score; matching endpoint nodes are fallback evidence only when the relationship text has no match. Semantic recall always includes relationship-text evidence alongside matching activation bundles: exact relationship evidence outweighs keyword evidence, keyword weight is multiplied by the fraction of bounded query terms present in the fact text, and endpoint-only fallback contributes no semantic score. Activation cosine is normalized from zero at its admission threshold to at most the keyword weight, repeated activation bundles cannot amplify a fact by accumulation, and direct evidence wins an exact score tie. The returned `score` is the final fused score. Learned bundles retain up to three facts grounded by an exact relationship match or complete keyword coverage, in fused order, while admitting at most two from one exact subject-and-property group when another group is available. Partial keyword coverage can rank but cannot teach. The lexical results provide a cold start for untouched topics, and later related queries can reuse and refresh the learned bundle. Activation-only results cannot create or rewrite a bundle, preventing recursive semantic chaining. A semantic query that resolves no facts creates no activation.

Grounded direct or activation facts can also surface bounded visible one-hop `ASSERTS` neighbors as ephemeral structural context. Structural scores are capped below their anchor and penalized by endpoint degree and repeated anchor-property groups. Only the strongest route is retained, facts with direct or activation evidence receive no structural boost, and expanded facts never enter or update activation bundles. This makes graph connections useful without letting popular endpoints teach semantic fan-out. Scope, labels, current-fact checks, and endpoint closure still apply; `ABOUT` remains excluded.

Ordinary recall stays inside Neo4j and is read-only. Semantic recall sends only the query text, up to 32 KiB UTF-8, to the configured embedding provider and maintains expiring semantic activations. Use it only when conceptual matching is necessary and that disclosure is acceptable.

```json
{
  "text": "Alice",
  "scope": ["project:graph-memory", "team:agents"],
  "detail": "concise"
}
```

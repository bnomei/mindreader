---
name: writing-to-mindreader
description: Decides whether a fact belongs in mindreader graph memory and how to assert it as one NODE-REL-NODE triple. Use when about to persist memory, write to mindreader, or choose between chat context and the graph.
---

# Writing to mindreader

Write only what another agent should find next week. Chat-only useful stays in chat.

MCP server: `user-mindreader`. Always call `user-mindreader:<tool>`.

## Write if

- Identity: who or what something is
- Decision: a choice that should constrain later work
- How it works: a durable operating fact
- Contention: two current values that disagree

## Do not write

- Task status or acknowledgements
- Prose, markdown dumps, or a paragraph in one literal
- A new property when search already shows a similar one
- Schema unless search shows the Class or Property is missing

## Steps

1. Choose `@layers`, the request's `layers` array. Use one or more named memberships for this work; use `[]` only for global memory.
2. Use `user-mindreader:memory_search` for an exact name or relation. Use `user-mindreader:memory_semantic_search` for a concept when sending the query text to the configured OpenAI or xAI embedding API is acceptable. Multiple layers are an OR union. If the fact is already current, stop.
3. One triple per `user-mindreader:memory_assert`. Use tagged entity/literal objects for `s` and `o`; `p` is a relation; always pass `layers`.
4. Layer IDs use lowercase kebab-case colon namespaces, for example `project:graph-memory` or `analysis:hypothesis`. Colons are naming, not hierarchy.
5. `spike=Knowledge` only if you would bet on it. Otherwise `Signal` (raw) or `Insight` (interpreted). Do not auto-promote.
6. Add another valid value with `memory_assert`. Correct selected memberships of one exact old value with `memory_replace`; use `memory_retract` only to withdraw.
7. If another visible membership has a different current `(s,p)`, set `contradicts=true`.
8. Inspect `mergeSuggestions` returned by `memory_assert`, `memory_replace`, and `memory_schema`. A suggestion is only a fuzzy same-kind name match, not proof of identity. If—and only if—the entities are truly identical, pass its `merge` payload to `memory_merge`; reverse `source` and `target` when the other IRI/name should survive.
9. Search again for `s` or the topic. If the new fact is missing, fix the assert.
10. After using a retrieved node or relationship, call `memory_feedback`: `strengthen` if it helped, `weaken` if it did not. Retrieval never updates weight itself.

## Examples

Good:

```
s={kind:entity,name:Alice}  p=worksOn  o={kind:entity,name:mindreader}  layers=[project:graph-memory]  spike=Knowledge
s={kind:entity,name:Bob}    p=INSTANCE_OF  o={kind:entity,iri:mindreader:class/Agent}  layers=[]
```

Bad:

```
s=Alice  p=INSTANCE_OF  o=mindreader:element/agent   # Agent is a Class, not an Element
s=Bob    p=notes  o="I am working on the MCP today"  # task status, not durable
```

## Read

| Situation | Tool |
| --- | --- |
| No IRI yet | `user-mindreader:memory_search` |
| Conceptual recall; external embedding is acceptable | `user-mindreader:memory_semantic_search` |
| Have an IRI | `user-mindreader:memory_get` |
| Walk typed edges | `user-mindreader:memory_traverse` |
| Retrieved target helped or hurt | `user-mindreader:memory_feedback` |
| Audit or correct memberships | `user-mindreader:memory_layers` |
| New Class or Property after search misses | `user-mindreader:memory_schema` |
| Confirmed duplicate entities | `user-mindreader:memory_merge` |

Every read filters nodes, relationships, and relationship endpoints through the same `@layers` scope. Empty stored memberships are global. One exact relationship identity is shared across memberships, so asserting it again merges memberships. Feedback is explicit `+1`/`-1`, shared across memberships, has no decay, and ranks results only within the same Spike category.

Semantic search returns the same lightweight current facts with a 1-based `rank`. It blends direct results with nearby expiring bundles of relationship IRIs stored in Neo4j; it never bypasses the request's layers, labels, or endpoint closure. Calling it sends only the query text to the selected embedding provider. Similar searches can converge and refresh a 30-day TTL without creating an Episode or changing feedback weights.

`memory_merge` is global, permanent, and has no `layers` input. It removes the source after moving all memberships and current and historical relationships to the target; the target IRI and name survive. It creates one Episode but no alias. The shorter-name direction in `mergeSuggestions` is merely a default—similar strings such as `007` and `007s` may be distinct, so the agent must make the identity decision.

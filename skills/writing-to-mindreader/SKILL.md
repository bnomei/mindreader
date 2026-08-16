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

1. Choose `scope`, the request's visibility array. Use one or more named memberships for this work; use `[]` only for global memory.
2. Use `user-mindreader:memory_recall` with `text` for an exact name. Set `semantic:true` for a concept when sending the query text to the configured embedding API is acceptable. Catalog Classes/Properties with `labels: ["Class"]` or `["Property"]`. If the fact is already current, stop.
3. One `user-mindreader:memory_write` call with `facts[]` (1–20 triples) and call-level `scope`. Use tagged `node`/`literal` objects for each fact's `s` and `o`; `p` is a relation.
4. Layer IDs use lowercase kebab-case colon namespaces, for example `project:graph-memory` or `analysis:hypothesis`. Colons are naming, not hierarchy.
5. `spike=Knowledge` only if you would bet on it. Otherwise `Signal` (raw) or `Insight` (interpreted). Do not auto-promote.
6. Add another valid value with `memory_write`. Correct one current fact by pasting its `target` into `memory_revise`. Use `memory_withdraw` only to withdraw.
7. If another visible membership has a different current `(s,p)`, set that fact's `contradicts=true`.
8. Inspect `next.unify` on the write result. A suggestion is only a fuzzy same-kind name match, not proof of identity. If—and only if—the nodes are truly identical, pass `{source,target}` to `memory_unify`.
9. Recall again for `s` or the topic. If the new fact is missing, fix the write.
10. After using a retrieved node or fact, call `memory_judge` with `strengthen` or `weaken`. Retrieval never updates weight itself.

## Examples

Good:

```
memory_write facts=[{s:{kind:node,name:Alice}, p=worksOn, o:{kind:node,name:mindreader}, spike=Knowledge}] scope=[project:graph-memory]
memory_write facts=[{s:{kind:node,name:Bob}, p=INSTANCE_OF, o:{kind:node,iri:mindreader:class/Agent}}] scope=[]
```

Bad:

```
s=Alice  p=INSTANCE_OF  o=mindreader:element/agent   # Agent is a Class, not an Element
s=Bob    p=notes  o="I am working on the MCP today"  # task status, not durable
```

## Read

| Situation | Tool |
| --- | --- |
| No IRI yet | `user-mindreader:memory_recall` with `text` |
| Conceptual recall; external embedding is acceptable | `user-mindreader:memory_recall` with `semantic:true` |
| Have an IRI | `user-mindreader:memory_recall` with `iris` |
| Walk edges by predicate name | `user-mindreader:memory_recall` with `around` and `p` |
| Catalog Class or Property | `user-mindreader:memory_recall` with `labels: ["Class"]` or `["Property"]` |
| Retrieved target helped or hurt | `user-mindreader:memory_judge` |
| Change memberships | `user-mindreader:memory_place` |
| Confirmed duplicate nodes | `user-mindreader:memory_unify` |

Every read filters nodes, facts, and endpoints through the same `scope`. Empty stored memberships are global. One exact fact identity is shared across memberships, so writing it again merges memberships. Judge is explicit `+1`/`-1`, shared across memberships, has no decay, and ranks results only within the same Spike category.

`memory_recall` with `semantic:true` returns the same lightweight current facts with a 1-based `rank`. It blends direct results with nearby expiring bundles stored in Neo4j; it never bypasses the request's scope or endpoint closure. Calling it sends only the query text to the selected embedding provider.

`memory_unify` is global, permanent, and has no `scope` input. Source and target must have the same single canonical kind. It removes the source after moving all memberships and current and historical relationships to the target; the target IRI and name survive. The shorter-name direction in `next.unify` is only a default—similar strings such as `007` and `007s` may be distinct, so the agent must make the identity decision.

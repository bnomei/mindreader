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
2. `user-mindreader:memory_search` for the thing and relation in that scope. Multiple names are an OR union. If the fact is already current, stop.
3. One triple per `user-mindreader:memory_assert`. Use tagged entity/literal objects for `s` and `o`; `p` is a relation; always pass `layers`.
4. Layer IDs use lowercase kebab-case colon namespaces, for example `project:graph-memory` or `analysis:hypothesis`. Colons are naming, not hierarchy.
5. `spike=Knowledge` only if you would bet on it. Otherwise `Signal` (raw) or `Insight` (interpreted). Do not auto-promote.
6. Add another valid value with `memory_assert`. Correct selected memberships of one exact old value with `memory_replace`; use `memory_retract` only to withdraw.
7. If another visible membership has a different current `(s,p)`, set `contradicts=true`.
8. Search again for `s` or the topic. If the new fact is missing, fix the assert.
9. After using a retrieved node or relationship, call `memory_feedback`: `strengthen` if it helped, `weaken` if it did not. Retrieval never updates weight itself.

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
| Have an IRI | `user-mindreader:memory_get` |
| Walk typed edges | `user-mindreader:memory_traverse` |
| Retrieved target helped or hurt | `user-mindreader:memory_feedback` |
| Audit or correct memberships | `user-mindreader:memory_layers` |
| New Class or Property after search misses | `user-mindreader:memory_schema` |

Every read filters nodes, relationships, and relationship endpoints through the same `@layers` scope. Empty stored memberships are global. One exact relationship identity is shared across memberships, so asserting it again merges memberships. Feedback is explicit `+1`/`-1`, shared across memberships, has no decay, and ranks results only within the same Spike category.

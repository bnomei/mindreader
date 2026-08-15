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

1. `user-mindreader:memory_search` for the thing and the relation. If the fact is already current, stop.
2. One triple per `user-mindreader:memory_assert`. `s` and `o` are things. `p` is a relation.
3. Layer: `project:<slug>` for this work. `global` only if it should survive every project.
4. `spike=Knowledge` only if you would bet on it. Otherwise `Signal` (raw) or `Insight` (interpreted). Do not auto-promote.
5. Correct a value by asserting the new `o` (supersedes). Retract only to withdraw.
6. If another visible layer has a different current `(s,p)`, set `contradicts=true`.
7. Search again for `s` or the topic. If the new fact is missing, fix the assert.

## Examples

Good:

```
s=Alice  p=worksOn  o=mindreader  layer=project:graph-memory  spike=Knowledge
s=Bob    p=INSTANCE_OF  o=mindreader:class/Agent
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
| New Class or Property after search misses | `user-mindreader:memory_schema` |

---
name: using-mindreader
description: "Operates Mindreader durable graph memory through scoped recall, semantic recall, fact writes, corrections, withdrawals, feedback, layer placement, and duplicate-node unification. Use whenever an agent should retrieve or maintain durable user, project, team, or task context through the Mindreader MCP server, including work that may depend on remembered decisions, facts, or identities."
---

# Using Mindreader

Use only the stages the task needs. Most tasks need one recall and no mutation; a durable update usually needs at most one targeted recall and one batched mutation.

Always call tools by the host-exposed fully qualified name; never emit a bare tool name in a call plan or instruction. The canonical server name is `mindreader`, as in `mindreader:memory_recall`; if a host configures an alias, replace only that prefix. Treat each tool's advertised schema and description as the syntax authority. Do not make preflight calls merely to discover payload fields.

## Default workflow

1. Choose the request's narrowest applicable `scope`.
2. Reuse facts, node IRIs, and `target` handles already in context. Otherwise make one recall using the selector that directly matches the need.
3. Use the available evidence. If durable state must change, choose the smallest correct mutation and batch related items.
4. Preserve returned handles, trust a successful structured result, and stop.

Add work only when it can change the decision:

- Skip recall before an idempotent exact write when the fact, identities, predicate, and scope are already unambiguous. Recall before corrections, withdrawals, identity-sensitive changes, or when current state matters.
- Do not cascade lexical, semantic, catalog, IRI, and neighborhood recalls. Stop when one result is sufficient.
- Inspect a nonempty review queue only when its candidates are relevant and you may act on them. Never create follow-up work solely because a queue exists.
- Verify a successful mutation only when the response does not establish the postcondition, the change is broad or consequential, or the commit outcome is unknown.
- Judge only retrievals that materially helped or misled. Do not rate every result or recall solely to create a rating.

## Scope and identity

Every tool except `mindreader:memory_unify` requires `scope`.

- `[]` selects global records only. Named scopes see global records plus records whose memberships intersect any requested layer.
- On `mindreader:memory_write`, `scope` becomes the membership of new ordinary nodes and facts. Exact reassertion merges named memberships unless the record is already global; global membership dominates and remains global. Writing an exact identity or fact with `scope: []` makes it global, so use `[]` deliberately.
- Use returned node IRIs for known entities and returned fact `target` handles for mutations. Never reconstruct a fact IRI or treat a fuzzy match as identity.
- Layer IDs use lowercase kebab-case colon namespaces such as `project:graph-memory`. Colons are naming, not hierarchy. There is no process-wide default scope.
- Layers are visibility filters, not tenant isolation or authorization. Any MCP caller can request any valid layer.

## Choose one operation

| Intent | Tool |
| --- | --- |
| Read visible lexical, IRI, catalog, neighborhood, or history data | `mindreader:memory_recall` |
| Match a concept when lexical recall is insufficient and provider disclosure is acceptable | `mindreader:memory_recall_semantic` |
| Add durable current facts or merge memberships | `mindreader:memory_write` |
| Correct one exact current fact | `mindreader:memory_revise` |
| Soft-remove a fact or subject/predicate slice without replacement | `mindreader:memory_withdraw` |
| Change memberships without changing facts | `mindreader:memory_place` |
| Rate a recalled node or current fact | `mindreader:memory_judge` |
| Permanently merge two confirmed same-kind identities | `mindreader:memory_unify` |

Facts are set-valued. A different object for the same subject and predicate is another current value unless evidence establishes that the old value is wrong. Add a valid parallel value with `mindreader:memory_write`; correct a wrong value with `mindreader:memory_revise`; remove a stale value without replacement with `mindreader:memory_withdraw`.

Set `contradicts: true` only when a visible current alternative for the same subject and predicate is directly incompatible and both claims should remain current. It links the new object to the conflicting visible current objects. Do not recall solely to populate this flag. Never assert, revise, or withdraw the system-owned `CONTRADICTS` or `SUPERSEDES` relationships directly.

For exact recall selectors, semantic disclosure, detail modes, and limits, read [references/recall.md](references/recall.md). Always read [references/mutations.md](references/mutations.md) before revise, withdraw, place, judge, or unify; also read it before using contradiction or review-queue features on a write.

## Write durable facts, not conversation

Store durable identities and relationships, decisions that constrain future work, stable operating knowledge, reusable signals or insights, and explicit contention that matters later.

Do not store task chatter, acknowledgements, transient status, secrets, or prose/markdown dumps. Class/Property records and schema-definition edges are global.

## Handle outcomes

Successful mutations include `ok: true`, `noop`, and `episode`; scoped tools echo `scope`. `mindreader:memory_judge` and `mindreader:memory_place` add an input-ordered `items` list and `summary`. An all-noop mutation returns `episode: null`. Preserve returned `handles`; after `mindreader:memory_revise`, use `target` or `handles.current`, not `previousTarget` or `handles.retired`.

Recoverable failures return `ok: false`, `reason`, `message`, `retryable`, and `outcome`:

- Retry only when `retryable: true` and `outcome: "not_applied"`, honoring `retryAfterMs`.
- If `outcome: "unknown"`, do not repeat the mutation. Make one targeted recall when the postcondition is observable; otherwise report uncertainty. Never repeat `mindreader:memory_judge` or `mindreader:memory_unify` merely because their effect cannot be confirmed.
- Fix invalid input or failed preconditions instead of retrying unchanged.
- Treat every batch as atomic; after an error, never assume earlier items applied.

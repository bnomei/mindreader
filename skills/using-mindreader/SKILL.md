---
name: using-mindreader
description: "Provides autonomous selective graph memory for agents. Use when work may depend on prior stored decisions, preferences, constraints, identities, relationships, conventions, commitments, project state, or lessons, or may produce supported knowledge worth reusing. Handles proactive recall, explicit capture, correction, withdrawal, feedback, layer placement, and identity merging through Mindreader."
---

# Using Mindreader

Mindreader is selective prospective memory, not conversation storage. You are the clerk: decide what deserves retention and make explicit tool calls. Mindreader performs no hidden extraction. Details you do not assert are intentionally unavailable later.

Own the lifecycle without waiting for “remember.” Recall when prior memory can affect the work; capture supported knowledge chosen for future reuse; maintain it when evidence changes. A check may correctly produce no call.

Use host-exposed fully qualified tool names such as `mindreader:recall`. Treat each advertised schema and tool description as the syntax authority.

## Work loop

1. Use the narrowest applicable `scope`. Copy a task- or host-supplied scope exactly.
2. Recall at the start of resumed or context-sensitive work, and before a consequential decision that prior knowledge could change. Reuse facts, IRIs, and handles already in context; otherwise make one targeted recall.
3. Do the work. Treat recalled relationships as evidence only for what they assert.
4. When work establishes durable reusable knowledge, select standalone supported triples and make the smallest correct mutation. Batch related items.
5. Keep returned handles, trust a successful structured result, and stop.

Do not cascade recall modes. Retry once only for a plausible terminology or neighborhood miss; use semantic recall only when conceptual matching is needed and provider disclosure is acceptable. Do not verify a successful mutation unless its result leaves the postcondition unclear or the change is broad or consequential.

## Choose one operation

| Intent | Tool |
| --- | --- |
| Read lexical, IRI, catalog, neighborhood, or history assertions | `mindreader:recall` |
| Match a concept after lexical recall is insufficient | `mindreader:recall_semantic` |
| Add selected facts or merge memberships | `mindreader:write` |
| Correct one exact current fact | `mindreader:revise` |
| Soft-remove current facts without replacement | `mindreader:withdraw` |
| Change layer memberships without changing meaning | `mindreader:place` |
| Rate retrieval utility | `mindreader:judge` |
| Permanently merge two confirmed same-kind identities | `mindreader:unify` |

## Scope and handles

Every tool except `mindreader:unify` requires `scope`. `[]` selects global records only. A named scope sees global records plus records in any requested layer; multiple names form an OR union. Layers control visibility, not authorization. Changing a supplied scope ID selects a different layer.

Reuse returned node IRIs for established identities and fact `target` handles for exact mutations. Never reconstruct a fact IRI or treat a fuzzy suggestion as identity.

## Recall

Use one mode that directly matches the need:

| Need | Call |
| --- | --- |
| Search entity, predicate, or object terms | `mindreader:recall` with `text` |
| Fetch known node identities | `mindreader:recall` with `iris` |
| Inspect facts by endpoint label or the Class/Property catalog | `mindreader:recall` with `labels` |
| Walk relationships from a known node | `mindreader:recall` with `around` |
| Inspect current and retired facts or revisions | `mindreader:recall` with `history` |
| Match meaning after lexical recall is insufficient | `mindreader:recall_semantic` |

`mindreader:recall` accepts exactly one selector. Use only fields that apply to it.

- `text` works best with concrete entity and relationship terms likely to occur in a stored triple.
- `iris` accepts node IRIs, preserves input order, reports misses, and returns incident facts per lookup. Use `hops` only when top-level one-hop facts are also needed.
- `labels` selects facts in ordinary recall; Class or Property selects the global catalog. In semantic recall, labels only filter results.
- `around` starts at a node IRI. Direction, depth, and predicate filters constrain every traversed edge. Omit a predicate filter until stored vocabulary is known. Paths witness returned facts; they are not additional assertions.
- `history` accepts one node or exact fact IRI and returns current and retired facts plus revision events. Supersession edges are audit metadata, not mutation targets.

Use `detail:"concise"` for answer-bearing content. Use `detail:"detailed"` when handles, memberships, ranking, mutability, rateability, paths, or audit context can affect the next action. A global fact visible through a named scope is not mutable there.

ABOUT is explicit classified context. Detailed text recall may return it separately in `about[]`; it is not an ordinary, neighborhood, history, or semantic fact.

Start with one direct selector and stop when it supplies enough evidence. Prefer known IRIs over text. Inspect the subject, predicate, and object: a matching endpoint or nearby node does not prove another relationship. If a deliberately stored assertion may use different wording, retry once with concrete entity terms, a close synonym, or an unfiltered neighborhood from a known node.

Missing memory remains unknown. An empty result means no matching assertion is stored and visible for that query and scope; it does not search omitted conversation details or prove an event did not occur.

### Effective and semantic recall

`effectiveAt` is a state-as-of filter. It returns only transaction-current ordinary facts whose explicit half-open effective interval contains the requested instant; unknown-time facts are excluded. Do not use it merely because a question mentions a date. Recall point events and calendar-date relationships normally. Use `history` for transaction revisions; it rejects `effectiveAt` because it exposes both clocks.

Use `mindreader:recall_semantic` only when the remaining gap is conceptual and provider disclosure is acceptable. Only query text is sent to the configured embedding provider. The call combines lexical evidence with expiring semantic activations and may add weaker one-hop context. Context aids navigation but is not independent proof and never teaches activations. The call may create or refresh activations, so it is externally disclosing, non-read-only, and non-idempotent; an empty result creates none. Ordinary recall stays closed-world, read-only, and inside Neo4j.

## Select what to retain

Write only when the candidate is:

- useful beyond the current turn;
- a precise standalone relationship with stable identities and vocabulary;
- supported at its chosen commitment level;
- assigned the correct scope; and
- safe to retain.

Good candidates include durable decisions and rationale, preferences and standing instructions, requirements and invariants, identities and relationships, conventions, commitments, stable project state, reusable operational knowledge, and deliberately classified evidence or insights.

Do not store raw conversation, prose or source dumps, secrets, transient progress, temporary paths, build logs, expiring one-off instructions, or unsupported inference. Do not mirror an authoritative artifact when future work can derive the fact more safely and cheaply from that artifact.

Recall searches explicit graph assertions, not source conversations or documents. `mindreader:write` does not ingest conversation or infer companion facts.

### Write facts precisely

Facts are set-valued. A different object or effective interval is another current fact; newer evidence is not automatically a replacement. Write compatible parallel values. An exact subject, predicate, object, effective qualification, and interval has one identity: reassertion merges named memberships or is a no-op.

Reuse established node and Property IRIs. Use names only when identity is unambiguous, and use the same predicate for the same concept.

Spike is commitment on one exact fact:

- `Signal`: reusable raw evidence;
- `Pattern`: recurring observation;
- `Insight`: supported interpretation;
- `Knowledge`: a fact worth relying on.

Omit Spike rather than overstate evidence. Do not auto-promote. Reassertion and revision preserve an existing Spike unless one is explicitly supplied.

Set `contradicts:true` only when the new fact and visible current alternatives are directly incompatible and both should remain current. It records contradiction links; it does not replace alternatives. `review.alternatives` is advisory. Spike never creates ABOUT; write ABOUT only for genuine classified context. Never write, revise, or withdraw the system-owned CONTRADICTS or SUPERSEDES predicates directly.

### Model effective time

Transaction time records when memory changed. Effective time records when an ordinary state held in the represented world. Its interval is half-open `[from,to)`, uses timezone-qualified RFC 3339 bounds, and may be open on either side. Omitted or null effective metadata means unknown world time; an empty object is explicitly qualified and open in both directions.

Use effective intervals for states such as residence, employment, ownership, status, or configuration. Model a point or repeatable occurrence as an event node with participant and date/time facts. Use an `xsd:date` literal when only a calendar date is known; never invent midnight or a zero-length interval. Resolve relative dates only from trusted dated context.

## Maintain memory

### Revise or withdraw

Use a retained fact handle or one targeted detailed recall. Do not treat chronological arrival as proof that an older fact is wrong.

`mindreader:revise` corrects one exact current fact when its object or interval is wrong and a replacement is known. Its scope moves only selected memberships; unrelated values and memberships remain. The subject and predicate do not change. Omitted `effective` inherits the interval, null clears it, and an object replaces it. Revision soft-closes the selected fact and records SUPERSEDES atomically.

Use `mindreader:withdraw` when no replacement is known. Prefer an exact target. Subject form removes every mutable visible outgoing fact in the selected slice; add `p` only to narrow that slice and use subject form only when that breadth is intentional. Withdrawal is soft and preserves history.

A global fact visible through a named scope cannot be revised or withdrawn there; use `scope:[]` only when changing the global fact is intended. Removing the final selected fact membership retires it rather than making it global.

### Place memberships

Use `mindreader:place` only to change visibility membership without changing meaning. Request scope selects visible targets; each edit supplies memberships to add or remove. Facts must remain visible through both endpoints in the batch's final state. When moving a fact into a layer an endpoint lacks, edit that endpoint in the same batch; persisted literal endpoints use their returned node handles.

Removing a node or fact's final named membership makes it global. To move between named layers without global exposure, add the destination and remove the source in one edit.

### Unify identities

`review.unify` contains fuzzy same-kind candidates, not proof. Use `mindreader:unify` only when independent evidence confirms both nodes are the same identity and which IRI and name must survive. Source is permanently absorbed into target across the database. There is no scope, alias, or undo.

### Judge retrieval utility

Use `mindreader:judge` only after a recalled node or current fact materially helped, distracted, or misled actual work. Strengthen adds exactly +1 shared weight; weaken adds exactly -1. Weight is retrieval feedback, not confidence, truth, or recency. Correct or withdraw false knowledge instead of merely weakening it. Do not rate every result or recall solely to create feedback.

## Handle results

Mutation batches are atomic and record at most one Episode; an all-noop mutation records none. Retry only when `retryable` is true and honor `retryAfterMs`; otherwise fix the input or precondition rather than repeating the call. Never repeat non-idempotent `judge` or permanent `unify` merely because their effect is uncertain.

---
name: using-mindreader
description: "Provides autonomous durable graph memory for agents. Use proactively—without waiting for a memory request—when starting, continuing, investigating, deciding, implementing, debugging, reviewing, or completing work that may depend on prior decisions, preferences, constraints, identities, relationships, conventions, commitments, project state, or lessons, or may produce future-useful facts, rationale, patterns, or insights. Also handles explicit remember, forget, and correction requests through the Mindreader MCP server."
---

# Using Mindreader

Mindreader is the agent's working memory, not a user-operated notebook. Own the memory lifecycle: proactively recall context before it can affect the work and proactively capture durable knowledge as the work produces or confirms it. Do not wait for the user to say "remember" or ask whether a qualifying fact should be saved. Explicit remember, forget, and correction requests are strong inputs, not the normal trigger.

Mindreader itself never extracts facts from conversation. You are the clerk: decide what is future-useful, formulate a supported subject-predicate-object assertion, choose its scope and commitment level, and make the explicit tool call. Discussion is a capture trigger when it establishes durable knowledge even if nobody uses memory-related words.

Use only the stages the task needs. A capture check can correctly conclude that no recall or mutation is warranted; autonomous does not mean storing everything.

Always call tools by the host-exposed fully qualified name; never emit a bare tool name in a call plan or instruction. The canonical server name is `mindreader`, as in `mindreader:recall`; if a host configures an alias, replace only that prefix. Treat each tool's advertised schema and description as the syntax authority. Do not make preflight calls merely to discover payload fields.

## Autonomous trigger policy

| Agent situation | Memory action |
| --- | --- |
| Starting, resuming, or revisiting work where prior context could change the approach | Recall relevant decisions, preferences, constraints, identities, conventions, commitments, project state, and lessons |
| Before a consequential choice or an identity-sensitive mutation | Recall enough current or historical context to avoid contradicting earlier knowledge |
| Discussion, investigation, implementation, debugging, or review establishes reusable knowledge | Write concise facts, decisions and rationale, requirements, relationships, or deliberately classified signals/patterns/insights |
| New evidence shows one current value is wrong or no longer true | Recall its exact fact handle, then revise it when a replacement is known or withdraw it when none is |
| A recalled target materially improves or harms the work | Judge its retrieval value; revise or withdraw instead when the stored claim is false |
| The appropriate project, team, or other visibility membership changes | Place the existing node or fact without changing its meaning |
| Evidence confirms that two nodes are the same identity | Unify only after confirming which node must survive |
| Completing a significant task or preparing a handoff | Perform a capture check for durable outcomes, decisions, constraints, unresolved commitments, and reusable lessons; batch related changes |

## Default workflow

1. Choose the work's narrowest applicable `scope`.
2. At an autonomous recall trigger, reuse facts, node IRIs, and `target` handles already in context. Otherwise make one recall using the selector that directly matches the need. Use `detail:"concise"` for answer-only reading and `detail:"detailed"` when a later operation or audit may need handles, memberships, ranking, or eligibility.
3. Do the work. As durable knowledge emerges, reduce it to precise supported triples rather than conversation excerpts.
4. At an autonomous capture or maintenance trigger, choose the smallest correct mutation and batch related items.
5. Preserve returned handles, trust a successful structured result, and stop.

Add work only when it can change the decision:

- Skip recall before an idempotent exact write when the fact, identities, predicate, and scope are already unambiguous. Recall before corrections, withdrawals, identity-sensitive changes, or when current state matters.
- Do not cascade lexical, semantic, catalog, IRI, and neighborhood recalls. Stop when one result is sufficient.
- Inspect a nonempty review queue only when its candidates are relevant and you may act on them. Never create follow-up work solely because a queue exists.
- Verify a successful mutation only when the response does not establish the postcondition or the change is broad or consequential.
- Judge only retrievals that materially helped or misled. Do not rate every result or recall solely to create a rating.

## Scope and identity

Every tool except `mindreader:unify` requires `scope`.

- `[]` selects global records only. Named scopes see global records plus records whose memberships intersect any requested layer.
- On `mindreader:write`, `scope` becomes the membership of new ordinary nodes and facts. Exact reassertion merges named memberships unless the record is already global; global membership dominates and remains global. Writing an exact identity or fact with `scope: []` makes it global, so use `[]` deliberately.
- Use returned node IRIs for known entities and returned fact `target` handles for mutations. Never reconstruct a fact IRI or treat a fuzzy match as identity.
- Layer IDs use lowercase kebab-case colon namespaces such as `project:graph-memory`. Colons are naming, not hierarchy. There is no process-wide default scope.
- Layers are visibility filters, not tenant isolation or authorization. Any MCP caller can request any valid layer.

## Choose one operation

| Intent | Tool |
| --- | --- |
| Read visible lexical, IRI, catalog, neighborhood, or history data | `mindreader:recall` |
| Match a concept when lexical recall is insufficient and provider disclosure is acceptable | `mindreader:recall_semantic` |
| Add durable current facts or merge memberships | `mindreader:write` |
| Correct one exact current fact | `mindreader:revise` |
| Soft-remove a fact or subject/predicate slice without replacement | `mindreader:withdraw` |
| Change memberships without changing facts | `mindreader:place` |
| Rate a recalled node or current fact | `mindreader:judge` |
| Permanently merge two confirmed same-kind identities | `mindreader:unify` |

Facts are set-valued. A different object for the same subject and predicate is another current value unless evidence establishes that the old value is wrong. Add a valid parallel value with `mindreader:write`; correct a wrong value with `mindreader:revise`; remove a stale value without replacement with `mindreader:withdraw`.

`spike` classifies one exact fact; it never labels an endpoint or creates `ABOUT`. Write `ABOUT` explicitly only for genuine context. Explicit ABOUT can appear in detailed recall's `about[]`, but never as an ordinary recalled fact or semantic activation result.

Set `contradicts: true` only when a visible current alternative for the same subject and predicate is directly incompatible and both claims should remain current. It links the new object to the conflicting visible current objects. Do not recall solely to populate this flag. Never assert, revise, or withdraw the system-owned `CONTRADICTS` or `SUPERSEDES` relationships directly.

For exact recall selectors, semantic disclosure, detail modes, and limits, read [references/recall.md](references/recall.md). Always read [references/mutations.md](references/mutations.md) before revise, withdraw, place, judge, or unify; also read it before using contradiction or review-queue features on a write.

## Capture future-useful knowledge, not conversation

A memory candidate may come from the user, the agent's own reasoning, tool output, repository evidence, or the completed work. Store it only when another agent or session is likely to need it and the current evidence supports the assertion and its Spike level.

Capture:

- identities, ownership, and durable relationships;
- preferences and standing operating instructions that should alter future agent behavior;
- decisions, selected or rejected alternatives, and rationale that constrains later work;
- requirements, invariants, compatibility boundaries, and other durable constraints;
- conventions, stable project or environment facts, and reusable operational knowledge;
- durable commitments, unresolved blockers, and handoff state that must survive this task; and
- reusable raw evidence, recurring observations, or interpretations deliberately classified as Signal, Pattern, or Insight rather than overstated as Knowledge.

Before writing, ask internally: will this matter beyond the current turn, can it be expressed as a precise triple, is it supported at the selected commitment level, is its scope correct, and is it safe to store? If not, leave it in the conversation or authoritative project artifact.

- Do not store greetings, acknowledgements, reasoning chatter, or raw conversation.
- Do not store transient progress, temporary paths, build logs, disposable environment state, or one-off instructions that expire with the task.
- Do not store secrets, credentials, tokens, or authentication material.
- Do not dump prose, Markdown, source files, or documents into literals. Store only the concise durable assertion that future work needs.
- Do not mirror facts that are cheaper and safer to derive from an authoritative artifact unless the durable memory captures a decision, constraint, relationship, or interpretation that the artifact does not make obvious.
- Do not present unsupported inference as Knowledge. Either omit it or intentionally store the reusable evidence or interpretation at the appropriate lower Spike level.

Class/Property records and schema-definition edges are global.

## Handle results

Successful mutations include `ok: true`, `noop`, and `episode`; scoped tools echo `scope`. `mindreader:judge` and `mindreader:place` add an input-ordered `items` list and `summary`. An all-noop mutation returns `episode: null`. Preserve returned `handles`; after `mindreader:revise`, use `target` or `handles.current`, not `previousTarget` or `handles.retired`.

Recoverable failures return `ok: false`, `reason`, `message`, and `retryable`:

- Retry only when `retryable: true`, honoring `retryAfterMs`.
- Never repeat `mindreader:judge` or `mindreader:unify` after a non-retryable failure merely because their effect cannot be confirmed.
- Fix invalid input or failed preconditions instead of retrying unchanged.
- Treat every batch as atomic; after an error, never assume earlier items applied.
